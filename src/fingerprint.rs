//! Lectura del árbol `target/<profile>/.fingerprint/` que mantiene Cargo.
//!
//! Cargo escribe, por cada unidad de compilación, un directorio
//! `<paquete>-<hash>/` que contiene:
//!
//! ```text
//! sqlparser-3aed8a7b2a1da43b/
//!   invoked.timestamp        <- se toca en cada build que usa la unidad
//!   lib-sqlparser            <- hash de fingerprint, en hexadecimal
//!   lib-sqlparser.json       <- features, rustflags, perfil y aristas a deps
//!   dep-lib-sqlparser        <- dep-info binario (no lo usamos)
//! ```
//!
//! El `<hash>` del nombre del directorio es el mismo que Cargo pasa a rustc
//! vía `-C extra-filename`, así que mapea directo contra `deps/lib<crate>-<hash>.rlib`.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Una unidad de compilación tal como la registra Cargo.
#[derive(Debug, Clone)]
pub struct Unit {
    /// Nombre del paquete, sin el sufijo de hash: `sqlparser`.
    pub pkg: String,
    /// Nombre del archivo que identifica la unidad: `lib-sqlparser`, `test-lib-foo`.
    pub stem: String,
    /// Sufijo de hash del directorio; coincide con el de `deps/`.
    pub filename_hash: String,
    /// Hash de fingerprint que otras unidades usan para referenciar a esta.
    pub fp_hash: Option<u64>,
    /// Aristas hacia dependencias: `(nombre, hash_de_fingerprint)`.
    pub deps: Vec<(String, u64)>,
    /// mtime de `invoked.timestamp`: última vez que un build usó la unidad.
    pub invoked: Option<SystemTime>,
}

/// Un perfil compilado: el par `.fingerprint/` + `deps/` que viven juntos.
#[derive(Debug)]
pub struct Profile {
    /// Etiqueta legible, p.ej. `debug` o `x86_64-unknown-linux-musl/release`.
    pub label: String,
    /// El directorio del perfil: el padre de `.fingerprint/` y `deps/`.
    pub dir: PathBuf,
    /// La fase sweep tendrá que recolectar también este directorio: acumula
    /// unidades obsoletas al mismo ritmo que `deps/`.
    #[allow(dead_code)]
    pub fingerprint_dir: PathBuf,
    pub deps_dir: PathBuf,
    pub units: Vec<Unit>,
}

/// Localiza todos los perfiles compilados bajo un `target/`.
///
/// Busca directorios `.fingerprint` que tengan un `deps/` hermano, lo que cubre
/// tanto `target/debug/` como `target/<triple>/<profile>/`.
pub fn discover_profiles(target: &Path) -> Result<Vec<Profile>> {
    let mut found = Vec::new();
    collect_fingerprint_dirs(target, 0, &mut found)?;

    let mut profiles = Vec::new();
    for fp_dir in found {
        let parent = match fp_dir.parent() {
            Some(p) => p,
            None => continue,
        };
        let deps_dir = parent.join("deps");
        if !deps_dir.is_dir() {
            continue;
        }
        let label = parent
            .strip_prefix(target)
            .unwrap_or(parent)
            .to_string_lossy()
            .into_owned();

        let units = read_units(&fp_dir)
            .with_context(|| format!("leyendo unidades de {}", fp_dir.display()))?;

        profiles.push(Profile {
            label,
            dir: parent.to_path_buf(),
            fingerprint_dir: fp_dir,
            deps_dir,
            units,
        });
    }
    profiles.sort_by(|a, b| a.label.cmp(&b.label));
    Ok(profiles)
}

/// Recorre `target/` en busca de directorios `.fingerprint`, sin bajar dentro de ellos.
fn collect_fingerprint_dirs(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) -> Result<()> {
    if depth > 3 {
        return Ok(());
    }
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()), // permisos o carrera: lo saltamos en silencio
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        match path.file_name().and_then(|n| n.to_str()) {
            Some(".fingerprint") => out.push(path),
            // Nuestra propia papelera: dentro hay perfiles enteros ya barridos.
            // Tratarlos como perfiles vivos infla el informe y, peor, los vuelve a
            // planificar para barrer.
            Some(crate::sweep::TRASH_DIR) => {}
            // No tiene sentido descender en estos.
            Some("deps") | Some("build") | Some("incremental") | Some(".rustc_info.json") => {}
            _ => collect_fingerprint_dirs(&path, depth + 1, out)?,
        }
    }
    Ok(())
}

/// Lee todas las unidades de un directorio `.fingerprint/`.
fn read_units(fingerprint_dir: &Path) -> Result<Vec<Unit>> {
    let mut units = Vec::new();

    let entries = match fs::read_dir(fingerprint_dir) {
        Ok(e) => e,
        Err(_) => return Ok(units),
    };

    for entry in entries.flatten() {
        let unit_dir = entry.path();
        if !unit_dir.is_dir() {
            continue;
        }
        let dir_name = match unit_dir.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        // El hash es el último segmento tras `-`; el resto es el nombre del paquete.
        let (pkg, filename_hash) = match dir_name.rsplit_once('-') {
            Some((p, h)) if is_hash(h) => (p.to_string(), h.to_string()),
            _ => continue,
        };

        let invoked = fs::metadata(unit_dir.join("invoked.timestamp"))
            .and_then(|m| m.modified())
            .ok();

        // Cada unidad se identifica por un par `<kind>-<crate>` + su `.json`.
        let inner = match fs::read_dir(&unit_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for file in inner.flatten() {
            let name = match file.file_name().into_string() {
                Ok(n) => n,
                Err(_) => continue,
            };
            if name == "invoked.timestamp"
                || name.ends_with(".json")
                || name.starts_with("dep-")
                || name.starts_with("output-")
            {
                continue;
            }
            let json_path = unit_dir.join(format!("{name}.json"));
            if !json_path.is_file() {
                continue;
            }

            let fp_hash = fs::read_to_string(file.path())
                .ok()
                .and_then(|s| parse_cargo_hash(s.trim()));
            let deps = read_deps(&json_path).unwrap_or_default();

            units.push(Unit {
                pkg: pkg.clone(),
                stem: name.clone(),
                filename_hash: filename_hash.clone(),
                fp_hash,
                deps,
                invoked,
            });
        }
    }
    Ok(units)
}

/// Extrae las aristas `deps` del JSON de fingerprint.
///
/// El formato es una lista de tuplas heterogéneas; según la versión de Cargo
/// puede ser `[pkg_hash, nombre, public, fp_hash]` o `[pkg_hash, nombre, fp_hash]`.
/// Tomamos el nombre y el **último** número, que siempre es el fingerprint.
fn read_deps(json_path: &Path) -> Result<Vec<(String, u64)>> {
    let raw = fs::read_to_string(json_path)?;
    let value: serde_json::Value = serde_json::from_str(&raw)?;

    let list = match value.get("deps").and_then(|d| d.as_array()) {
        Some(l) => l,
        None => return Ok(Vec::new()),
    };

    let mut deps = Vec::new();
    for item in list {
        let tuple = match item.as_array() {
            Some(t) => t,
            None => continue,
        };
        let name = tuple
            .iter()
            .find_map(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let fp = tuple.iter().rev().find_map(|v| v.as_u64());
        if let Some(fp) = fp {
            deps.push((name, fp));
        }
    }
    Ok(deps)
}

/// Separa `lib-sqlparser` en `("lib", "sqlparser")`.
///
/// Hoy las raíces se agrupan por el nombre de archivo completo, que es más
/// robusto; esto queda para cuando la fase sweep necesite distinguir tipos de
/// unidad (p.ej. barrer sólo los `test-*`).
#[allow(dead_code)]
///
/// Los prefijos de tipo son un conjunto cerrado que Cargo conoce; el resto del
/// nombre es el crate, que puede contener guiones.
fn split_kind(file_name: &str) -> (String, String) {
    const KINDS: &[&str] = &[
        "run-build-script-build-script-build",
        "run-build-script-build-script-main",
        "build-script-build-script-build",
        "build-script-build-script-main",
        "run-build-script",
        "build-script-build",
        "build-script-main",
        "test-build-script-build",
        "test-lib",
        "test-bin",
        "bench-lib",
        "bench-bin",
        "doc-lib",
        "lib",
        "bin",
        "test",
        "bench",
        "example",
        "custom-build",
    ];
    for kind in KINDS {
        if let Some(rest) = file_name.strip_prefix(kind) {
            if rest.is_empty() {
                return ((*kind).to_string(), String::new());
            }
            if let Some(name) = rest.strip_prefix('-') {
                return ((*kind).to_string(), name.to_string());
            }
        }
    }
    match file_name.split_once('-') {
        Some((k, n)) => (k.to_string(), n.to_string()),
        None => (file_name.to_string(), String::new()),
    }
}

/// Decodifica un hash de fingerprint tal como lo escribe Cargo.
///
/// Cargo serializa con `hex::encode(num.to_le_bytes())`, es decir **little-endian**.
/// Leerlo como un entero hexadecimal normal da un número distinto y ninguna
/// arista del grafo resuelve.
pub fn parse_cargo_hash(s: &str) -> Option<u64> {
    if s.len() != 16 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = [0u8; 8];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(u64::from_le_bytes(bytes))
}

/// ¿Parece un sufijo de hash de Cargo? (hex, 8 caracteres o más)
fn is_hash(s: &str) -> bool {
    s.len() >= 8 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Índice de `fp_hash -> unidades`, para resolver las aristas del grafo.
pub fn index_by_fp_hash(units: &[Unit]) -> HashMap<u64, Vec<usize>> {
    let mut map: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, u) in units.iter().enumerate() {
        if let Some(h) = u.fp_hash {
            map.entry(h).or_default().push(i);
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separa_tipo_y_crate() {
        assert_eq!(
            split_kind("lib-sqlparser"),
            ("lib".into(), "sqlparser".into())
        );
        assert_eq!(
            split_kind("test-lib-app_core"),
            ("test-lib".into(), "app_core".into())
        );
        assert_eq!(
            split_kind("build-script-build"),
            ("build-script-build".into(), String::new())
        );
        // Un crate con guiones no debe partirse por la mitad.
        assert_eq!(
            split_kind("lib-aws-sdk-glue"),
            ("lib".into(), "aws-sdk-glue".into())
        );
    }

    #[test]
    fn decodifica_hash_little_endian() {
        // Valor real tomado del target/debug/.fingerprint de un proyecto real: el dep `log`
        // que sqlparser declara como 237437814390372343 se escribe f7b7fb266f8c4b03.
        assert_eq!(
            parse_cargo_hash("f7b7fb266f8c4b03"),
            Some(237437814390372343)
        );
        assert_eq!(parse_cargo_hash("no-es-hex"), None);
        assert_eq!(parse_cargo_hash("abc"), None);
    }

    #[test]
    fn reconoce_sufijos_de_hash() {
        assert!(is_hash("3aed8a7b2a1da43b"));
        assert!(!is_hash("glue"));
        assert!(!is_hash("1.0"));
    }
}
