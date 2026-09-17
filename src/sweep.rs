//! Fase **sweep**: convertir el conjunto muerto en espacio libre.
//!
//! La fase mark decide *qué* sobra; acá se decide *cómo* se saca, que es donde
//! están los riesgos reales: no pisar un build en curso y no borrar de forma
//! irreversible a la primera.

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Directorio de papelera, relativo a la raíz de `target/`.
pub const TRASH_DIR: &str = ".reap-trash";

/// Qué hacer con lo que la fase mark declaró muerto.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Sólo informar. Es el modo por defecto.
    DryRun,
    /// Mover a la papelera, de donde se purga pasados los días de retención.
    Trash,
    /// Borrar directamente, sin red de seguridad.
    Delete,
}

/// Una entrada muerta: un archivo de `deps/` o un directorio completo de
/// `.fingerprint/` o `build/`.
#[derive(Debug)]
pub struct Entry {
    pub path: PathBuf,
    pub bytes: u64,
}

/// Plan de barrido de un perfil, antes de ejecutar nada.
#[derive(Debug, Default)]
pub struct Plan {
    pub deps: Vec<Entry>,
    pub fingerprints: Vec<Entry>,
    pub builds: Vec<Entry>,
    pub incremental: Vec<Entry>,
    pub codegen: Vec<Entry>,
}

impl Plan {
    pub fn bytes(&self) -> u64 {
        let sum = |v: &Vec<Entry>| v.iter().map(|e| e.bytes).sum::<u64>();
        sum(&self.deps)
            + sum(&self.fingerprints)
            + sum(&self.builds)
            + sum(&self.incremental)
            + sum(&self.codegen)
    }

    pub fn len(&self) -> usize {
        self.deps.len()
            + self.fingerprints.len()
            + self.builds.len()
            + self.incremental.len()
            + self.codegen.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn todas(&self) -> impl Iterator<Item = &Entry> {
        self.deps
            .iter()
            .chain(&self.fingerprints)
            .chain(&self.builds)
            .chain(&self.incremental)
            .chain(&self.codegen)
    }
}

/// Construye el plan de un perfil a partir del conjunto vivo de hashes.
///
/// `deps/` se clasifica archivo a archivo; `.fingerprint/` y `build/` por
/// directorio, porque ahí cada unidad ocupa un directorio propio cuyo sufijo de
/// hash es el mismo que el del artefacto.
pub fn plan_profile(
    profile_dir: &Path,
    vivos: &HashSet<String>,
    incluir_incremental: bool,
    incluir_codegen: bool,
) -> Result<Plan> {
    let mut plan = Plan::default();

    let deps = profile_dir.join("deps");
    for (path, bytes) in archivos_muertos(&deps, vivos)? {
        plan.deps.push(Entry { path, bytes });
    }

    // Los objetos de codegen de unidades *vivas*: rustc los reemite cuando toca
    // recompilar esa unidad, y Cargo no los cuenta entre los outputs que verifica,
    // así que borrarlos no ensucia el fingerprint ni fuerza un rebuild.
    if incluir_codegen {
        for (path, bytes) in codegen_vivos(&deps, vivos)? {
            plan.codegen.push(Entry { path, bytes });
        }
    }

    for (path, bytes) in dirs_muertos(&profile_dir.join(".fingerprint"), vivos)? {
        plan.fingerprints.push(Entry { path, bytes });
    }
    for (path, bytes) in dirs_muertos(&profile_dir.join("build"), vivos)? {
        plan.builds.push(Entry { path, bytes });
    }

    if incluir_incremental {
        let inc = profile_dir.join("incremental");
        if inc.is_dir() {
            let bytes = peso_recursivo(&inc);
            if bytes > 0 {
                plan.incremental.push(Entry { path: inc, bytes });
            }
        }
    }

    Ok(plan)
}

/// Archivos de `deps/` cuyo sufijo de hash no está en el conjunto vivo.
fn archivos_muertos(deps: &Path, vivos: &HashSet<String>) -> Result<Vec<(PathBuf, u64)>> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(deps) {
        Ok(e) => e,
        Err(_) => return Ok(out),
    };
    for entry in entries.flatten() {
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.is_file() {
            continue;
        }
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue,
        };
        // Sin hash reconocible no se toca: puede ser un artefacto que Cargo no
        // versiona y que no sabemos atribuir a ninguna unidad.
        if let Some(h) = crate::hash_de_artefacto(&name) {
            if !vivos.contains(&h) {
                out.push((entry.path(), meta.len()));
            }
        }
    }
    Ok(out)
}

/// Objetos `.rcgu.o` que pertenecen a unidades vivas.
///
/// Los de unidades muertas ya salen por [`archivos_muertos`]; acá se recogen los
/// que sobrevivirían, que es donde está el grueso: en un `target/debug/deps`
/// real medido son 11.49 GB, la mitad del directorio.
fn codegen_vivos(deps: &Path, vivos: &HashSet<String>) -> Result<Vec<(PathBuf, u64)>> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(deps) {
        Ok(e) => e,
        Err(_) => return Ok(out),
    };
    for entry in entries.flatten() {
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.is_file() {
            continue;
        }
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue,
        };
        if !name.ends_with(".rcgu.o") {
            continue;
        }
        // Los muertos ya los recogió `archivos_muertos`; sin hash reconocible
        // (`app.<id>.<cgu>.rcgu.o`) también son intermedios y entran acá.
        let vivo = match crate::hash_de_artefacto(&name) {
            Some(h) => vivos.contains(&h),
            None => true,
        };
        if vivo {
            out.push((entry.path(), meta.len()));
        }
    }
    Ok(out)
}

/// Subdirectorios `<paquete>-<hash>` cuyo hash no está en el conjunto vivo.
fn dirs_muertos(dir: &Path, vivos: &HashSet<String>) -> Result<Vec<(PathBuf, u64)>> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(out),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue,
        };
        let hash = match name.rsplit_once('-') {
            Some((_, h)) if h.len() >= 8 && h.chars().all(|c| c.is_ascii_hexdigit()) => h,
            _ => continue,
        };
        if !vivos.contains(hash) {
            let bytes = peso_recursivo(&path);
            out.push((path, bytes));
        }
    }
    Ok(out)
}

/// Suma el tamaño de todo lo que cuelga de un directorio.
fn peso_recursivo(dir: &Path) -> u64 {
    let mut total = 0u64;
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return 0,
    };
    for entry in entries.flatten() {
        match entry.metadata() {
            Ok(m) if m.is_file() => total += m.len(),
            Ok(m) if m.is_dir() => total += peso_recursivo(&entry.path()),
            _ => {}
        }
    }
    total
}

/// Resultado de ejecutar un plan.
#[derive(Default, Debug)]
pub struct Stats {
    pub liberados: u64,
    pub entradas: usize,
    pub fallos: usize,
}

/// Ejecuta el plan. En [`Mode::DryRun`] no toca nada.
pub fn execute(plan: &Plan, target_root: &Path, mode: Mode) -> Result<Stats> {
    let mut stats = Stats::default();
    if mode == Mode::DryRun {
        return Ok(stats);
    }

    // Un único directorio de papelera por ejecución, con marca de tiempo, para
    // que la purga por retención pueda razonar por lote y no archivo a archivo.
    let lote = if mode == Mode::Trash {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let dir = target_root.join(TRASH_DIR).join(ts.to_string());
        fs::create_dir_all(&dir).with_context(|| format!("creando {}", dir.display()))?;
        Some(dir)
    } else {
        None
    };

    for entry in plan.todas() {
        let ok = match &lote {
            Some(lote) => mover_a_papelera(&entry.path, target_root, lote),
            None => borrar(&entry.path),
        };
        if ok {
            stats.liberados += entry.bytes;
            stats.entradas += 1;
        } else {
            stats.fallos += 1;
        }
    }
    Ok(stats)
}

/// Mueve preservando la ruta relativa dentro de `target/`, para que un rescate
/// manual sea copiar el árbol de vuelta.
fn mover_a_papelera(path: &Path, target_root: &Path, lote: &Path) -> bool {
    let rel = match path.strip_prefix(target_root) {
        Ok(r) => r,
        Err(_) => return false,
    };
    let destino = lote.join(rel);
    if let Some(padre) = destino.parent() {
        if fs::create_dir_all(padre).is_err() {
            return false;
        }
    }
    // Dentro del mismo sistema de archivos esto es un rename: instantáneo y
    // sin copiar bytes.
    fs::rename(path, &destino).is_ok()
}

fn borrar(path: &Path) -> bool {
    if path.is_dir() {
        fs::remove_dir_all(path).is_ok()
    } else {
        fs::remove_file(path).is_ok()
    }
}

/// Purga los lotes de papelera más viejos que la retención. Devuelve bytes liberados.
pub fn purgar_papelera(target_root: &Path, retener: Duration) -> u64 {
    let trash = target_root.join(TRASH_DIR);
    let entries = match fs::read_dir(&trash) {
        Ok(e) => e,
        Err(_) => return 0,
    };
    let ahora = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut liberados = 0u64;
    for entry in entries.flatten() {
        let path = entry.path();
        let ts = match entry
            .file_name()
            .into_string()
            .ok()
            .and_then(|n| n.parse::<u64>().ok())
        {
            Some(t) => t,
            None => continue,
        };
        if ahora.saturating_sub(ts) > retener.as_secs() {
            let bytes = peso_recursivo(&path);
            if fs::remove_dir_all(&path).is_ok() {
                liberados += bytes;
            }
        }
    }
    let _ = fs::remove_dir(&trash); // sólo tiene efecto si quedó vacío
    liberados
}

/// ¿Hay un build de Cargo en curso sobre este perfil?
///
/// Cargo mantiene `<profile>/.cargo-lock` tomado mientras compila. Intentamos un
/// `flock` exclusivo no bloqueante: si falla, hay un build corriendo y no se
/// debe tocar nada, porque estaríamos borrando artefactos que rustc está
/// escribiendo en ese mismo momento.
pub fn build_en_curso(profile_dir: &Path) -> bool {
    use std::os::unix::io::AsRawFd;

    let lock = profile_dir.join(".cargo-lock");
    let file = match fs::File::open(&lock) {
        Ok(f) => f,
        // Sin archivo de lock nunca hubo build acá: no hay nada que respetar.
        Err(_) => return false,
    };

    // SAFETY: `flock` sobre un descriptor válido y vivo durante toda la llamada.
    // Es la única forma de consultar el mismo lock que usa Cargo; no hay
    // equivalente en std.
    let tomado = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if tomado == 0 {
        // Lo conseguimos: no había build. Lo soltamos enseguida.
        // SAFETY: mismo descriptor, todavía abierto.
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        false
    } else {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_vacio_no_tiene_bytes() {
        let p = Plan::default();
        assert!(p.is_empty());
        assert_eq!(p.bytes(), 0);
    }

    #[test]
    fn sin_lock_no_hay_build_en_curso() {
        let dir = std::env::temp_dir().join("cargo-reap-test-sin-lock");
        let _ = fs::create_dir_all(&dir);
        assert!(!build_en_curso(&dir));
    }

    #[test]
    fn dry_run_no_toca_nada() -> Result<()> {
        let raiz = std::env::temp_dir().join("cargo-reap-test-dryrun");
        let _ = fs::remove_dir_all(&raiz);
        fs::create_dir_all(raiz.join("deps"))?;
        let archivo = raiz.join("deps/libx-0123456789abcdef.rlib");
        fs::write(&archivo, b"contenido")?;

        let plan = Plan {
            deps: vec![Entry {
                path: archivo.clone(),
                bytes: 9,
            }],
            ..Default::default()
        };
        let stats = execute(&plan, &raiz, Mode::DryRun)?;
        assert_eq!(stats.entradas, 0);
        assert!(archivo.exists(), "dry-run no debe borrar");
        let _ = fs::remove_dir_all(&raiz);
        Ok(())
    }

    #[test]
    fn papelera_preserva_la_ruta_relativa() -> Result<()> {
        let raiz = std::env::temp_dir().join("cargo-reap-test-papelera");
        let _ = fs::remove_dir_all(&raiz);
        fs::create_dir_all(raiz.join("debug/deps"))?;
        let archivo = raiz.join("debug/deps/libx-0123456789abcdef.rlib");
        fs::write(&archivo, b"contenido")?;

        let plan = Plan {
            deps: vec![Entry {
                path: archivo.clone(),
                bytes: 9,
            }],
            ..Default::default()
        };
        let stats = execute(&plan, &raiz, Mode::Trash)?;
        assert_eq!(stats.entradas, 1);
        assert_eq!(stats.liberados, 9);
        assert!(!archivo.exists(), "el original debe haberse movido");

        // Debe quedar bajo <trash>/<ts>/debug/deps/...
        let lote = fs::read_dir(raiz.join(TRASH_DIR))?
            .flatten()
            .next()
            .map(|e| e.path())
            .context("no se creó el lote de papelera")?;
        assert!(lote.join("debug/deps/libx-0123456789abcdef.rlib").exists());

        let _ = fs::remove_dir_all(&raiz);
        Ok(())
    }

    #[test]
    fn la_purga_respeta_la_retencion() -> Result<()> {
        let raiz = std::env::temp_dir().join("cargo-reap-test-purga");
        let _ = fs::remove_dir_all(&raiz);
        let ahora = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();

        let viejo = raiz.join(TRASH_DIR).join((ahora - 86_400 * 30).to_string());
        let nuevo = raiz.join(TRASH_DIR).join(ahora.to_string());
        fs::create_dir_all(&viejo)?;
        fs::create_dir_all(&nuevo)?;
        fs::write(viejo.join("x"), b"12345")?;

        let liberados = purgar_papelera(&raiz, Duration::from_secs(86_400 * 7));
        assert_eq!(liberados, 5);
        assert!(!viejo.exists(), "el lote viejo debe purgarse");
        assert!(nuevo.exists(), "el lote reciente debe sobrevivir");

        let _ = fs::remove_dir_all(&raiz);
        Ok(())
    }
}
