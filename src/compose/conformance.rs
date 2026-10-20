//! nspawn compose against docker compose itself: the cases under tests/compose/cases
//! hold what the release the support table names prints for each run
//! (tests/compose/oracle.sh records it), and each run is made here, in this process,
//! and compared: the output byte for byte, the error, and the warnings, of which nspawn
//! may print more only where its support table speaks.

use std::path::{Path, PathBuf};

use std::cell::RefCell;

use super::config::{Host, Stdin};
use super::{gostr, support, Log};

struct Expected {
    stdout: String,
    warnings: Vec<String>,
    error: Option<String>,
}

/// A file of the case as Go reads it: its bytes, whatever they are.
fn read_go(path: &Path) -> String {
    std::fs::read(path)
        .map(|bytes| gostr::from_bytes(&bytes))
        .unwrap_or_default()
}

fn expected(dir: &Path, n: usize) -> Expected {
    let read = |ext: &str| read_go(&dir.join(format!("expected/{n}.{ext}")));
    let stderr = read("stderr");
    let mut warnings = Vec::new();
    let mut error = Vec::new();
    for line in stderr.lines() {
        // logrus under --progress json.
        if let Some(Ok(json)) = line
            .starts_with("{\"level\":\"warning\"")
            .then(|| serde_json::from_str::<serde_json::Value>(line))
        {
            warnings.push(json["msg"].as_str().unwrap_or_default().to_string());
        } else if let Some(rest) = line.strip_prefix("time=") {
            let msg = rest.split_once(" msg=").map(|(_, m)| m).unwrap_or("");
            warnings.push(gostr::unquote(msg));
        } else {
            error.push(line);
        }
    }
    let status: i32 = read("status").trim().parse().unwrap_or(0);
    // Where nspawn refuses on purpose what docker compose takes, the case says so.
    let nspawn = read_go(&dir.join(format!("nspawn-{n}.error")));
    let error = if !nspawn.is_empty() {
        Some(nspawn.trim_end_matches('\n').to_string())
    } else if status != 0 {
        Some(error.join("\n"))
    } else {
        None
    };
    Expected {
        stdout: read("stdout"),
        warnings,
        error,
    }
}

/// What nspawn compose writes besides its output, kept to compare.
#[derive(Default)]
struct Record {
    warnings: RefCell<Vec<String>>,
    notices: RefCell<String>,
}

impl Log for Record {
    fn warn(&self, text: String) {
        self.warnings.borrow_mut().push(text);
    }

    fn notice(&self, text: &str) {
        self.notices.borrow_mut().push_str(text);
    }

    fn json(&self) {}
}

/// The texts of the support table, which only nspawn prints.
fn table_texts() -> Vec<&'static str> {
    support::ATTRIBUTES
        .iter()
        .map(|a| a.support)
        .chain(support::VALUES.iter().map(|v| v.support))
        .chain(support::COMMANDS.iter().map(|c| c.support))
        .chain(support::OPTIONS.iter().map(|o| o.support))
        .filter_map(|s| s.text())
        .collect()
}

fn run(dir: &Path, n: usize, line: &str) -> Result<(), String> {
    let name = dir.file_name().unwrap().to_string_lossy().to_string();
    let virtual_dir = format!("/compose/{name}");
    let real_dir = dir.to_string_lossy().to_string();
    let words: Vec<String> = line.split_whitespace().map(String::from).collect();
    let mut env: super::dotenv::Env = [
        ("HOME", "/home/user"),
        ("PATH", "/usr/local/bin:/usr/bin:/bin"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let text = read_go(&dir.join("env"));
    for l in text
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        if let Some((k, v)) = l.split_once('=') {
            env.insert(k.to_string(), v.to_string());
        }
    }
    let stdin = std::fs::read(dir.join("stdin")).unwrap_or_default();
    let mut host = Host::new(env, real_dir.clone(), Stdin::Given(RefCell::new(stdin)));
    let record = Record::default();
    let mut out = Vec::new();
    // As the binary does it, but for the help, which is clap's.
    let result = match super::read(&words) {
        super::Line::Run(args) => super::execute(&args, &mut host, &mut out, &record),
        super::Line::Help(_) => Ok(()),
        super::Line::Error { message, .. } => Err(message),
    };
    // docker compose's notices come before its error, on the same stream.
    let result = result.map_err(|e| format!("{}{e}", record.notices.borrow()));
    let warnings = record.warnings.into_inner();
    let local = |text: &str| text.replace(&real_dir, &virtual_dir);
    let stdout = local(&gostr::from_bytes(&out));
    let error = result.err().map(|e| local(&e));
    let warnings: Vec<String> = warnings.iter().map(|w| local(w)).collect();
    let want = expected(dir, n);
    let mut problems = Vec::new();
    if error != want.error {
        problems.push(format!(
            "error:\n  docker: {:?}\n  nspawn: {:?}",
            want.error, error
        ));
    }
    // docker compose writes --variables in the order of a Go map, which is random.
    let unordered = |text: &str| {
        let mut lines: Vec<&str> = text.lines().collect();
        if line.contains("--variables") && !line.contains("json") && !line.contains("yaml") {
            lines[1..].sort();
        }
        // And --services in the order of a Go map, dependencies first, and the names
        // of --volumes, --networks, --models and --images in that of one too.
        if [
            "--services",
            "--volumes",
            "--networks",
            "--models",
            "--images",
        ]
        .iter()
        .any(|flag| line.contains(flag))
        {
            lines.sort();
        }
        lines.join("\n")
    };
    if error.is_none() && want.error.is_none() && unordered(&stdout) != unordered(&want.stdout) {
        problems.push(diff(&unordered(&want.stdout), &unordered(&stdout)));
    }
    let texts = table_texts();
    let ours: Vec<&String> = warnings
        .iter()
        .filter(|w| want.warnings.contains(w) || !texts.iter().any(|t| w.ends_with(t)))
        .collect();
    let mut a: Vec<&String> = want.warnings.iter().collect();
    let mut b = ours.clone();
    a.sort();
    b.sort();
    if a != b {
        problems.push(format!("warnings:\n  docker: {a:?}\n  nspawn: {b:?}"));
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(format!("{name} run {n} ({line}):\n{}", problems.join("\n")))
    }
}

/// The first lines where two outputs part.
fn diff(want: &str, got: &str) -> String {
    let (w, g): (Vec<&str>, Vec<&str>) = (want.lines().collect(), got.lines().collect());
    let first = w
        .iter()
        .zip(&g)
        .position(|(a, b)| a != b)
        .unwrap_or_else(|| w.len().min(g.len()));
    let start = first.saturating_sub(3);
    let show = |lines: &[&str]| {
        lines[start.min(lines.len())..(first + 6).min(lines.len())]
            .iter()
            .map(|l| format!("    {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "output differs at line {}:\n  docker:\n{}\n  nspawn:\n{}",
        first + 1,
        show(&w),
        show(&g)
    )
}

fn cases(root: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("expected").is_dir())
        .collect();
    dirs.sort();
    dirs
}

fn check(root: &Path) {
    let mut failures = Vec::new();
    let mut count = 0;
    for dir in cases(root) {
        let runs =
            std::fs::read_to_string(dir.join("runs")).unwrap_or_else(|_| "config".to_string());
        for (i, line) in runs.lines().filter(|l| !l.trim().is_empty()).enumerate() {
            count += 1;
            // On the stack the binary gives nspawn compose.
            let (dir, line) = (dir.clone(), line.to_string());
            let result = std::thread::Builder::new()
                .stack_size(super::STACK)
                .spawn(move || run(&dir, i + 1, &line))
                .unwrap()
                .join()
                .unwrap();
            if let Err(e) = result {
                failures.push(e);
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {count} runs differ from docker compose:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

#[test]
fn the_cases_match_docker_compose() {
    check(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/compose/cases"));
}

/// The same over cases kept outside the repository, compose files of other projects:
/// NSPAWN_COMPOSE_CORPUS=DIR cargo test corpus -- --ignored
#[test]
#[ignore]
fn the_corpus_matches_docker_compose() {
    let root = std::env::var("NSPAWN_COMPOSE_CORPUS").expect("NSPAWN_COMPOSE_CORPUS");
    check(Path::new(&root));
}
