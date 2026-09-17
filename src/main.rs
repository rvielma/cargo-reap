//! cargo-reap — recolector de basura para directorios `target/` de Cargo.
//!
//! Cargo nunca borra artefactos viejos: cada cambio de versión, feature o flag
//! escribe un archivo nuevo con otro sufijo de hash y deja el anterior ahí para
//! siempre. Este prototipo implementa la fase **mark**: reconstruir, leyendo
//! `.fingerprint/`, qué artefactos de `deps/` sigue necesitando el build actual.

mod fingerprint;
mod sweep;

use anyhow::{bail, Result};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    // Permite invocarlo como `cargo reap` además de `cargo-reap`.
    let args: Vec<&str> = args
        .iter()
        .skip(1)
        .filter(|a| a.as_str() != "reap")
        .map(|s| s.as_str())
        .collect();

    let mut path = PathBuf::from("target");
    // Por defecto se conservan TODAS las configuraciones del workspace. Podar por
    // recencia es inseguro: `invoked.timestamp` sólo avanza en las unidades que
    // Cargo recompila, así que la variante más reciente no es la que vas a
    // construir, sino la última que tocó una recompilación. Ver `raices()`.
    let mut keep_configs = usize::MAX;
    let mut list_dead = false;
    let mut mode = sweep::Mode::DryRun;
    let mut incremental = false;
    // Los .rcgu.o de unidades vivas se barren por defecto: son objetos
    // intermedios que rustc reemite cuando recompila la unidad, y Cargo no los
    // cuenta entre los outputs que verifica. Borrarlos no provoca ni un rebuild
    // extra, y son la mayor parte de lo recuperable.
    let mut codegen = true;
    let mut retain_days = 7u64;
    let mut it = args.iter().peekable();
    while let Some(arg) = it.next() {
        match *arg {
            "--keep-configs" => {
                let v = it.next().copied().unwrap_or("1");
                keep_configs = v.parse().unwrap_or(1).max(1);
                eprintln!(
                    "⚠ --keep-configs {keep_configs}: se descartarán configuraciones \
                     del workspace por antigüedad."
                );
                eprintln!(
                    "  Si la que construyes a continuación es una de ellas, el build \
                     recompila entero."
                );
            }
            "--list-dead" => list_dead = true,
            "--apply" => {
                if mode == sweep::Mode::DryRun {
                    mode = sweep::Mode::Trash;
                }
            }
            "--no-trash" => mode = sweep::Mode::Delete,
            "--incremental" => incremental = true,
            "--keep-codegen" => codegen = false,
            // Aceptado por compatibilidad: ya es el comportamiento por defecto.
            "--codegen" => codegen = true,
            "--retain-days" => {
                let v = it.next().copied().unwrap_or("7");
                retain_days = v.parse().unwrap_or(7);
            }
            "-h" | "--help" => {
                println!("uso: cargo reap <ruta-a-target> [--keep-configs N]");
                println!();
                println!("Marca como vivo todo artefacto alcanzable desde las unidades");
                println!("del workspace, siguiendo el grafo de .fingerprint/.");
                println!();
                println!("  --keep-configs N   conserva sólo las N configuraciones más");
                println!("                     recientes de cada unidad. Por defecto se");
                println!("                     conservan todas, que es lo seguro: podar");
                println!("                     por antigüedad puede borrar la config que");
                println!("                     usas y forzar un rebuild completo.");
                println!("  --list-dead        imprime la ruta de cada archivo muerto,");
                println!("                     una por línea, sin borrar nada.");
                println!("  --apply            mueve lo muerto a la papelera");
                println!("                     <target>/.reap-trash/. Sin esto, sólo informa.");
                println!("  --no-trash         con --apply, borra directo, sin papelera.");
                println!("  --retain-days N    purga los lotes de papelera con más de N");
                println!("                     días (por defecto 7).");
                println!("  --incremental      barre también incremental/. Es desechable");
                println!("                     por definición, pero deja el próximo build");
                println!("                     lento. Es la otra mitad del espacio.");
                println!("  --keep-codegen     NO barrer los .rcgu.o de unidades vivas.");
                println!("                     Por defecto sí se barren: son intermedios");
                println!("                     que rustc reemite y no cuestan un rebuild.");
                println!();
                println!("Si la ruta no es un target/, se buscan todos los target/");
                println!("que cuelguen de ella y se procesan uno por uno.");
                return Ok(());
            }
            other => path = PathBuf::from(other),
        }
    }

    if !path.is_dir() {
        bail!("no existe el directorio {}", path.display());
    }
    // Sin canonicalizar, el padre de una ruta relativa como `target` es la cadena
    // vacía y `cargo metadata` no tiene dónde ejecutarse.
    let path = fs::canonicalize(&path)?;

    let targets = resolver_targets(&path)?;
    if targets.is_empty() {
        bail!("no encontré ningún target/ compilado en {}", path.display());
    }
    let multi = targets.len() > 1;
    if multi && !list_dead {
        println!("{} proyectos bajo {}", targets.len(), path.display());
    }

    let opts = Opciones {
        keep_configs,
        list_dead,
        mode,
        incremental,
        codegen,
        retain_days,
        detallado: !multi,
    };

    let mut total = Totales::default();
    for t in &targets {
        match procesar_target(t, &opts) {
            Ok(r) => {
                if multi && !list_dead && r.plan > 0 {
                    let raiz = t.parent().unwrap_or(t);
                    let nombre = raiz
                        .strip_prefix(&path)
                        .unwrap_or(raiz)
                        .to_string_lossy()
                        .into_owned();
                    let nombre = if nombre.is_empty() {
                        raiz.file_name().map(|n| n.to_string_lossy().into_owned())
                    } else {
                        Some(nombre)
                    };
                    println!("  {:<44} {:>10}", nombre.unwrap_or_default(), human(r.plan));
                }
                total.sumar(&r);
            }
            // Un proyecto roto no debe abortar el recorrido de los demás.
            Err(e) => eprintln!("  ⚠ {}: {e:#}", t.display()),
        }
    }

    if list_dead {
        return Ok(());
    }

    println!("\n{}", "=".repeat(64));
    println!("recuperable   {:>10}", human(total.plan));
    match mode {
        sweep::Mode::DryRun => {
            println!("modo          simulación — no se tocó nada");
            println!("              usa --apply para moverlo a la papelera");
        }
        sweep::Mode::Trash => {
            println!(
                "liberado      {:>10}  (movido a {}/)",
                human(total.liberado),
                sweep::TRASH_DIR
            );
            println!("              se purga solo a los {retain_days} días");
        }
        sweep::Mode::Delete => {
            println!(
                "liberado      {:>10}  (borrado, sin papelera)",
                human(total.liberado)
            );
        }
    }
    if total.purgado > 0 {
        println!(
            "papelera      {:>10}  purgada por retención",
            human(total.purgado)
        );
    }
    if total.fallos > 0 {
        println!(
            "fallos        {} entradas no se pudieron mover",
            total.fallos
        );
    }
    if !total.ocupados.is_empty() {
        println!(
            "omitidos      {} (build en curso)",
            total.ocupados.join(", ")
        );
    }
    Ok(())
}

/// Ajustes de una corrida, comunes a todos los proyectos que se recorran.
struct Opciones {
    keep_configs: usize,
    list_dead: bool,
    mode: sweep::Mode,
    incremental: bool,
    codegen: bool,
    retain_days: u64,
    /// Con un solo proyecto se imprime el desglose por perfil; con varios, una
    /// línea por proyecto, que es lo que sirve en un cron.
    detallado: bool,
}

#[derive(Default)]
struct Totales {
    plan: u64,
    liberado: u64,
    purgado: u64,
    fallos: usize,
    ocupados: Vec<String>,
}

impl Totales {
    fn sumar(&mut self, otro: &Totales) {
        self.plan += otro.plan;
        self.liberado += otro.liberado;
        self.purgado += otro.purgado;
        self.fallos += otro.fallos;
        self.ocupados.extend(otro.ocupados.iter().cloned());
    }
}

/// Decide si la ruta es un `target/` o una carpeta con varios proyectos dentro.
fn resolver_targets(path: &Path) -> Result<Vec<PathBuf>> {
    if es_target(path) {
        return Ok(vec![path.to_path_buf()]);
    }
    let mut out = Vec::new();
    buscar_targets(path, 0, &mut out);
    out.sort();
    Ok(out)
}

/// ¿Este directorio es un `target/` de Cargo?
///
/// Cargo deja un `CACHEDIR.TAG` en la raíz de cada uno, que es la señal más
/// directa. No basta con buscar `.fingerprint` hacia abajo: desde una carpeta
/// que contiene varios proyectos también se encuentran, y el directorio entero
/// pasaría por un solo target.
fn es_target(path: &Path) -> bool {
    if path.join("CACHEDIR.TAG").is_file() {
        return true;
    }
    path.file_name().is_some_and(|n| n == "target")
        && path
            .parent()
            .is_some_and(|p| p.join("Cargo.toml").is_file())
}

/// Busca directorios `target/` que sean hermanos de un `Cargo.toml`.
fn buscar_targets(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 5 {
        return;
    }
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let nombre = entry.file_name();
        let nombre = nombre.to_string_lossy();
        if nombre == "target" {
            if es_target(&path) {
                out.push(path);
            }
            continue; // nunca descender dentro de un target
        }
        // Saltar sólo el ruido conocido. Descartar todo lo que empiece con punto
        // perdería los worktrees, que viven en directorios ocultos como `.teg/`.
        const RUIDO: &[&str] = &[
            ".git",
            ".gw",
            ".cargo",
            ".rustup",
            ".venv",
            ".tox",
            "node_modules",
            "vendor",
        ];
        if RUIDO.contains(&nombre.as_ref()) {
            continue;
        }
        buscar_targets(&path, depth + 1, out);
    }
}

/// Procesa un `target/`: marca, planifica y ejecuta.
fn procesar_target(path: &Path, opts: &Opciones) -> Result<Totales> {
    let mut total = Totales::default();

    let members = workspace_members(path)?;
    if members.is_empty() {
        bail!("no pude determinar los paquetes del workspace");
    }
    let profiles = fingerprint::discover_profiles(path)?;
    if profiles.is_empty() {
        return Ok(total);
    }

    if opts.detallado && !opts.list_dead {
        println!("target: {}", path.display());
        println!("workspace: {} paquetes", members.len());
    }

    // La papelera vencida se purga antes de nada: es espacio ya condenado y así
    // el informe de esta corrida no lo cuenta dos veces.
    if opts.mode != sweep::Mode::DryRun {
        total.purgado =
            sweep::purgar_papelera(path, Duration::from_secs(opts.retain_days * 86_400));
    }

    // El conjunto vivo se calcula una sola vez sobre todos los perfiles: las
    // aristas cruzan de perfil al cruzar-compilar. Ver [`conjunto_vivo`].
    let vivos = conjunto_vivo(&profiles, &members, opts.keep_configs);
    if opts.detallado && !opts.list_dead {
        vivos.print();
    }

    for profile in &profiles {
        let report = analyze(profile, &vivos, &members, opts.list_dead)?;

        if opts.list_dead {
            for f in &report.dead_paths {
                println!("{}", f.display());
            }
            continue;
        }
        if opts.detallado {
            report.print(&profile.label);
        }
        if report.sin_raices {
            continue;
        }

        // Nunca recolectar mientras Cargo compila: estaríamos borrando
        // artefactos que rustc está escribiendo en ese mismo instante.
        if sweep::build_en_curso(&profile.dir) {
            if opts.detallado {
                println!("   ⚠ hay un build en curso: este perfil no se toca");
            }
            total.ocupados.push(profile.label.clone());
            continue;
        }

        let plan =
            sweep::plan_profile(&profile.dir, &report.vivos, opts.incremental, opts.codegen)?;
        if opts.detallado {
            plan_resumen(&plan);
        }
        total.plan += plan.bytes();

        let stats = sweep::execute(&plan, path, opts.mode)?;
        total.liberado += stats.liberados;
        total.fallos += stats.fallos;
    }
    Ok(total)
}

/// Desglose de lo que el plan va a sacar, por zona del perfil.
fn plan_resumen(plan: &sweep::Plan) {
    if plan.is_empty() {
        return;
    }
    let linea = |nombre: &str, v: &Vec<sweep::Entry>| {
        if !v.is_empty() {
            let bytes: u64 = v.iter().map(|e| e.bytes).sum();
            println!(
                "   · {nombre:<14} {:>10}  ({} entradas)",
                human(bytes),
                v.len()
            );
        }
    };
    println!("   a recolectar:");
    linea("deps/", &plan.deps);
    linea(".fingerprint/", &plan.fingerprints);
    linea("build/", &plan.builds);
    linea("incremental/", &plan.incremental);
    linea("codegen .o", &plan.codegen);
}

/// Los paquetes del workspace y, por cada uno, los targets que declara hoy.
///
/// `.fingerprint/` no contiene ninguna de las dos cosas y ambas hacen falta:
///
/// - Qué crates son propios, porque son los que aportan raíces al grafo.
/// - Qué targets siguen declarados, porque una unidad huérfana —un binario
///   renombrado, un test de integración borrado— formaría si no un grupo de un
///   solo miembro que se auto-declara raíz y nunca se recolecta.
fn workspace_members(target: &Path) -> Result<HashMap<String, HashSet<String>>> {
    let raiz = target.parent().unwrap_or(target);
    if !raiz.join("Cargo.toml").is_file() {
        return Ok(HashMap::new());
    }
    let salida = std::process::Command::new("cargo")
        .args([
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--offline",
        ])
        .current_dir(raiz)
        .output();

    let salida = match salida {
        Ok(s) if s.status.success() => s,
        _ => return Ok(HashMap::new()),
    };
    let json: serde_json::Value = serde_json::from_slice(&salida.stdout)?;
    let paquetes = match json.get("packages").and_then(|p| p.as_array()) {
        Some(p) => p,
        None => return Ok(HashMap::new()),
    };

    let mut out: HashMap<String, HashSet<String>> = HashMap::new();
    for p in paquetes {
        let nombre = match p.get("name").and_then(|n| n.as_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let targets = p
            .get("targets")
            .and_then(|t| t.as_array())
            .map(|ts| {
                ts.iter()
                    .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
                    .map(normalizar)
                    .collect()
            })
            .unwrap_or_default();
        out.insert(nombre, targets);
    }
    Ok(out)
}

/// Cargo alterna guiones y guiones bajos según el contexto; para comparar
/// nombres hay que quedarse con una sola forma.
fn normalizar(s: &str) -> String {
    s.replace('-', "_")
}

/// ¿El nombre de archivo de esta unidad corresponde a un target vigente?
///
/// Los stems son `<tipo>-<target>`: `lib-app_core`,
/// `test-integration-test-schema_lifecycle`, `build-script-build`. Basta con
/// comprobar que terminan en un target declarado.
fn stem_vigente(stem: &str, targets: &HashSet<String>) -> bool {
    let stem = normalizar(stem);
    targets
        .iter()
        .any(|t| stem == *t || stem.ends_with(&format!("_{t}")))
}

/// Resultado del mark para un perfil.
struct Report {
    units: usize,
    live_bytes: u64,
    dead_bytes: u64,
    dead_files: usize,
    live_files: usize,
    huerfanos: usize,
    dead_paths: Vec<PathBuf>,
    sin_raices: bool,
    /// Sufijos de hash que la fase sweep debe preservar.
    vivos: HashSet<String>,
}

impl Report {
    fn print(&self, label: &str) {
        println!("\n── perfil {label}");
        if self.sin_raices {
            println!("   sin unidades del workspace: no se toca nada");
        }
        println!("   unidades en .fingerprint : {}", self.units);
        println!(
            "   deps/  vivo   {:>9}  ({} archivos)",
            human(self.live_bytes),
            self.live_files
        );
        println!(
            "   deps/  muerto {:>9}  ({} archivos)",
            human(self.dead_bytes),
            self.dead_files
        );
        if self.huerfanos > 0 {
            println!(
                "   archivos sin hash reconocible: {} (no se tocan)",
                self.huerfanos
            );
        }
    }
}

/// Fase **mark**: raíces por recencia + alcanzabilidad por el grafo de fingerprints.
///
/// Las dos mitades son necesarias y ninguna basta sola:
///
/// - Cargo solo toca `invoked.timestamp` de las unidades que **recompila**. Una
///   dependencia fresca conserva su fecha vieja, así que podar por antigüedad
///   (lo que hace `cargo-sweep`) borra artefactos vivos.
/// - El grafo por sí solo no distingue lo vivo de lo muerto: una variante obsoleta
///   sigue siendo alcanzable desde el padre obsoleto que la referenciaba.
///
/// La combinación sí funciona: las raíces recientes fijan la configuración actual
/// y el cierre transitivo rescata sus dependencias por viejas que sean.
/// Conjunto vivo calculado sobre **todos** los perfiles del `target/` a la vez.
///
/// Es imprescindible que sea global: al cruzar-compilar, el grafo abarca dos
/// directorios de perfil. Las unidades de `x86_64-unknown-linux-musl/release/`
/// dependen de proc-macros y build scripts que viven en el `release/` del host,
/// porque esos se compilan para la máquina que compila, no para el target.
/// Analizando perfil por perfil esas aristas no resuelven, el lado host parece
/// inalcanzable y se barre entero: en un workspace real eso marcaba como muertas
/// 150 de las 581 unidades que el build necesitaba.
struct Vivos {
    hashes: HashSet<String>,
    roots: usize,
    reachable: usize,
    edges_total: usize,
    edges_resolved: usize,
    anchor: Option<SystemTime>,
}

impl Vivos {
    fn print(&self) {
        let pct = if self.edges_total > 0 {
            100.0 * self.edges_resolved as f64 / self.edges_total as f64
        } else {
            0.0
        };
        println!(
            "grafo: {}/{} aristas resueltas ({:.1}%) · {} raíces -> {} unidades vivas",
            self.edges_resolved, self.edges_total, pct, self.roots, self.reachable
        );
        if let Some(a) = self.anchor {
            println!("actividad más reciente: {}", fecha(a));
        }
    }
}

fn conjunto_vivo(
    profiles: &[fingerprint::Profile],
    members: &HashMap<String, HashSet<String>>,
    keep_configs: usize,
) -> Vivos {
    // Un único vector con las unidades de todos los perfiles, y un índice de
    // fingerprint sobre él: así una arista puede cruzar de perfil sin perderse.
    let units: Vec<&fingerprint::Unit> = profiles.iter().flat_map(|p| p.units.iter()).collect();
    let mut by_fp: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, u) in units.iter().enumerate() {
        if let Some(h) = u.fp_hash {
            by_fp.entry(h).or_default().push(i);
        }
    }

    let mut edges_total = 0usize;
    let mut edges_resolved = 0usize;
    for u in &units {
        for (_, fp) in &u.deps {
            edges_total += 1;
            if by_fp.contains_key(fp) {
                edges_resolved += 1;
            }
        }
    }

    let propias: Vec<fingerprint::Unit> = units.iter().map(|u| (*u).clone()).collect();
    let roots = raices(&propias, members, keep_configs);

    let mut visto: HashSet<usize> = HashSet::new();
    let mut pila: Vec<usize> = roots.clone();
    while let Some(i) = pila.pop() {
        if !visto.insert(i) {
            continue;
        }
        for (_, fp) in &units[i].deps {
            if let Some(idxs) = by_fp.get(fp) {
                for &j in idxs {
                    if !visto.contains(&j) {
                        pila.push(j);
                    }
                }
            }
        }
    }

    Vivos {
        hashes: visto
            .iter()
            .map(|&i| units[i].filename_hash.clone())
            .collect(),
        roots: roots.len(),
        reachable: visto.len(),
        edges_total,
        edges_resolved,
        anchor: units.iter().filter_map(|u| u.invoked).max(),
    }
}

/// Clasifica el `deps/` de un perfil contra el conjunto vivo global.
fn analyze(
    profile: &fingerprint::Profile,
    vivos: &Vivos,
    members: &HashMap<String, HashSet<String>>,
    recolectar_rutas: bool,
) -> Result<Report> {
    let units = &profile.units;

    // Un perfil sin ninguna unidad del workspace no se toca: es más probable que
    // sea un perfil ajeno (sólo build scripts de dependencias) que basura.
    let del_workspace = units.iter().any(|u| {
        members
            .get(u.pkg.as_str())
            .is_some_and(|t| stem_vigente(&u.stem, t))
    });

    let (live_bytes, dead_bytes, live_files, dead_files, huerfanos, dead_paths) = if del_workspace {
        clasificar_deps_con_rutas(&profile.deps_dir, &vivos.hashes, recolectar_rutas)?
    } else {
        let (bytes, _, files, _, h) = clasificar_deps(&profile.deps_dir, &vivos.hashes, false)?;
        (bytes, 0, files, 0, h, Vec::new())
    };

    Ok(Report {
        units: units.len(),
        live_bytes,
        dead_bytes,
        live_files,
        dead_files,
        huerfanos,
        dead_paths,
        sin_raices: !del_workspace,
        vivos: vivos.hashes.clone(),
    })
}

/// Unidades del workspace que anclan el conjunto vivo.
///
/// Con `keep_configs` al máximo (el valor por defecto) devuelve todas las
/// variantes de cada `(paquete, tipo de unidad)` cuyo stem siga correspondiendo a
/// un target declarado hoy. Un valor menor poda por `invoked.timestamp`, que es
/// inseguro: ver el comentario del paso 2 en [`analyze`].
fn raices(
    units: &[fingerprint::Unit],
    members: &HashMap<String, HashSet<String>>,
    keep_configs: usize,
) -> Vec<usize> {
    let mut grupos: HashMap<(&str, &str), Vec<usize>> = HashMap::new();
    for (i, u) in units.iter().enumerate() {
        let targets = match members.get(u.pkg.as_str()) {
            Some(t) => t,
            None => continue,
        };
        if !stem_vigente(&u.stem, targets) {
            continue;
        }
        grupos
            .entry((u.pkg.as_str(), u.stem.as_str()))
            .or_default()
            .push(i);
    }
    let mut roots: Vec<usize> = Vec::new();
    for (_, mut grupo) in grupos {
        if keep_configs < grupo.len() {
            // Más reciente primero; sin timestamp va al final.
            grupo.sort_by_key(|&i| std::cmp::Reverse(units[i].invoked));
            grupo.truncate(keep_configs);
        }
        roots.extend(grupo);
    }
    roots
}

fn clasificar_deps(
    deps_dir: &Path,
    vivos: &HashSet<String>,
    recolectar: bool,
) -> Result<(u64, u64, usize, usize, usize)> {
    let (a, b, c, d, e, _) = clasificar_deps_con_rutas(deps_dir, vivos, recolectar)?;
    Ok((a, b, c, d, e))
}

/// Igual que [`clasificar_deps`], devolviendo además la ruta de cada archivo muerto.
fn clasificar_deps_con_rutas(
    deps_dir: &Path,
    vivos: &HashSet<String>,
    recolectar: bool,
) -> Result<(u64, u64, usize, usize, usize, Vec<PathBuf>)> {
    let mut live_bytes = 0u64;
    let mut dead_bytes = 0u64;
    let mut live_files = 0usize;
    let mut dead_files = 0usize;
    let mut huerfanos = 0usize;
    let mut dead_paths = Vec::new();

    let entries = match fs::read_dir(deps_dir) {
        Ok(e) => e,
        Err(_) => return Ok((0, 0, 0, 0, 0, dead_paths)),
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
        let size = meta.len();
        match hash_de_artefacto(&name) {
            Some(h) if vivos.contains(h.as_str()) => {
                live_bytes += size;
                live_files += 1;
            }
            Some(_) => {
                dead_bytes += size;
                dead_files += 1;
                if recolectar {
                    dead_paths.push(entry.path());
                }
            }
            None => huerfanos += 1,
        }
    }
    Ok((
        live_bytes, dead_bytes, live_files, dead_files, huerfanos, dead_paths,
    ))
}

/// Extrae el sufijo de hash de un artefacto de `deps/`.
///
/// El hash siempre está en el primer segmento, antes del primer punto: los nombres
/// de crate no pueden contener puntos, así que todo lo que sigue es extensión o
/// sufijo de codegen unit.
///
/// `libsqlparser-3aed8a7b2a1da43b.rlib`                 -> `3aed8a7b2a1da43b`
/// `app_server-32096233fdac0ecc`                        -> `32096233fdac0ecc`
/// `adler2-33c837f7cd28a9af.adler2.33f-cgu.0.rcgu.o`    -> `33c837f7cd28a9af`
fn hash_de_artefacto(name: &str) -> Option<String> {
    let stem = name.split_once('.').map(|(s, _)| s).unwrap_or(name);
    let (_, hash) = stem.rsplit_once('-')?;
    if hash.len() >= 8 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(hash.to_string())
    } else {
        None
    }
}

fn human(bytes: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.2} {}", U[i])
}

/// Formatea un instante como fecha local aproximada (días desde epoch).
fn fecha(t: SystemTime) -> String {
    let secs = match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(_) => return "?".into(),
    };
    let dias = secs / 86_400;
    let (y, m, d) = civil_from_days(dias);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Conversión de días desde epoch a fecha civil (algoritmo de Howard Hinnant).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extrae_hash_de_artefacto() {
        assert_eq!(
            hash_de_artefacto("libsqlparser-3aed8a7b2a1da43b.rlib").as_deref(),
            Some("3aed8a7b2a1da43b")
        );
        assert_eq!(
            hash_de_artefacto("app_server-32096233fdac0ecc").as_deref(),
            Some("32096233fdac0ecc")
        );
        assert_eq!(
            hash_de_artefacto("libaws_sdk_glue-e7411d878efa0c18.rmeta").as_deref(),
            Some("e7411d878efa0c18")
        );
        // Codegen units: el hash está antes del primer punto.
        assert_eq!(
            hash_de_artefacto("adler2-33c837f7cd28a9af.adler2.33fb6cfeba3d934d-cgu.0.rcgu.o")
                .as_deref(),
            Some("33c837f7cd28a9af")
        );
        // Sin sufijo de hash no se toca.
        assert_eq!(hash_de_artefacto("build"), None);
    }

    #[test]
    fn solo_los_targets_vigentes_son_raices() {
        let targets: HashSet<String> = ["cargo_reap".to_string()].into_iter().collect();
        // El binario declarado, en sus dos grafías.
        assert!(stem_vigente("bin-cargo-reap", &targets));
        assert!(stem_vigente("bin-cargo_reap", &targets));
        assert!(stem_vigente("test-bin-cargo-reap", &targets));
        // Un binario renombrado deja atrás una unidad que ya no corresponde a
        // ningún target: no debe volverse raíz de sí misma.
        assert!(!stem_vigente("bin-nombre-viejo", &targets));

        // Los build scripts se declaran como target propio.
        let bs: HashSet<String> = ["build_script_build".to_string()].into_iter().collect();
        assert!(stem_vigente("build-script-build", &bs));
        assert!(stem_vigente("run-build-script-build-script-build", &bs));
    }

    /// Construye un perfil mínimo con las unidades dadas.
    fn perfil(label: &str, units: Vec<fingerprint::Unit>) -> fingerprint::Profile {
        fingerprint::Profile {
            label: label.to_string(),
            dir: PathBuf::from(label),
            fingerprint_dir: PathBuf::from(label).join(".fingerprint"),
            deps_dir: PathBuf::from(label).join("deps"),
            units,
        }
    }

    /// Construye una unidad mínima para los tests de selección de raíces.
    fn unidad(pkg: &str, stem: &str, hash: &str, segundos: u64) -> fingerprint::Unit {
        fingerprint::Unit {
            pkg: pkg.to_string(),
            stem: stem.to_string(),
            filename_hash: hash.to_string(),
            fp_hash: None,
            deps: Vec::new(),
            invoked: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(segundos)),
        }
    }

    /// Regresión: dos configuraciones del mismo paquete del workspace coexisten
    /// (p. ej. `cargo build` y `cargo build --features x`). La más reciente es la
    /// del build con features, pero la que se va a construir después es la otra.
    /// Podar por fecha la barría y forzaba a recompilar el árbol entero.
    #[test]
    fn conserva_todas_las_configuraciones_del_workspace() {
        let units = vec![
            unidad("app", "lib-app", "aaaaaaaaaaaaaaaa", 100), // config sin features
            unidad("app", "lib-app", "bbbbbbbbbbbbbbbb", 200), // config con features, más nueva
        ];
        let mut members = HashMap::new();
        members.insert(
            "app".to_string(),
            ["app".to_string()].into_iter().collect::<HashSet<String>>(),
        );

        // Por defecto no se poda: ambas configuraciones anclan el conjunto vivo.
        let roots = raices(&units, &members, usize::MAX);
        assert_eq!(roots.len(), 2, "el default debe conservar ambas variantes");

        // Con --keep-configs 1 el usuario pide explícitamente podar, y se queda
        // sólo la más reciente — que no tiene por qué ser la que va a construir.
        let roots = raices(&units, &members, 1);
        assert_eq!(roots.len(), 1);
        assert_eq!(units[roots[0]].filename_hash, "bbbbbbbbbbbbbbbb");
    }

    /// Regresión: al cruzar-compilar, una unidad del target depende de un
    /// proc-macro que vive en el perfil del host. Si el grafo se calcula perfil
    /// por perfil, esa arista no resuelve y el proc-macro parece muerto.
    #[test]
    fn el_grafo_cruza_de_perfil() {
        let mut host = unidad("macro_derive", "lib-macro_derive", "1111111111111111", 10);
        host.fp_hash = Some(0xABCD);

        let mut target_unit = unidad("app", "lib-app", "2222222222222222", 20);
        target_unit.deps = vec![("macro_derive".to_string(), 0xABCD)];

        let perfiles = vec![
            perfil("x86_64-unknown-linux-musl/release", vec![target_unit]),
            perfil("release", vec![host]),
        ];
        let mut members = HashMap::new();
        members.insert(
            "app".to_string(),
            ["app".to_string()].into_iter().collect::<HashSet<String>>(),
        );

        let vivos = conjunto_vivo(&perfiles, &members, usize::MAX);
        assert_eq!(vivos.edges_resolved, 1, "la arista debe cruzar de perfil");
        assert!(
            vivos.hashes.contains("1111111111111111"),
            "el proc-macro del host tiene que quedar vivo"
        );
    }

    /// Una unidad cuyo paquete ya no está en el workspace no ancla nada, por más
    /// reciente que sea.
    #[test]
    fn un_paquete_ajeno_no_es_raiz() {
        let units = vec![unidad("ajeno", "lib-ajeno", "cccccccccccccccc", 999)];
        let members = HashMap::new();
        assert!(raices(&units, &members, usize::MAX).is_empty());
    }

    #[test]
    fn convierte_dias_a_fecha() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_000), (2024, 10, 4));
    }

    #[test]
    fn formatea_tamanos() {
        assert_eq!(human(0), "0.00 B");
        assert_eq!(human(1536), "1.50 KB");
    }
}
