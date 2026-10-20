use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::*;
use crate::build::image::{Healthcheck, History};
use crate::build::plan::{Action, Copy, From, Inline, Network, Run, RunMount, Step};

const PATH_ENV: &str = "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

fn amd64() -> Platform {
    Platform {
        os: "linux".to_string(),
        architecture: "amd64".to_string(),
        variant: None,
    }
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn image(cmd: &str, history: &[(&str, &str, bool)]) -> Config {
    let mut c = Config::scratch();
    c.architecture = "amd64".to_string();
    c.os = "linux".to_string();
    c.config.env = strings(&[PATH_ENV]);
    c.config.cmd = Some(strings(&[cmd]));
    c.rootfs.diff_ids = vec![format!("sha256:{}", "0".repeat(64))];
    c.history = history
        .iter()
        .map(|(created_by, comment, empty_layer)| History {
            created: None,
            created_by: created_by.to_string(),
            author: None,
            comment: comment.to_string(),
            empty_layer: *empty_layer,
        })
        .collect();
    c
}

/// busybox:1.37 as the goldens were recorded with it, the settings that matter.
fn busybox() -> Config {
    image("sh", &[("BusyBox 1.37.0 (glibc), Debian 13", "", false)])
}

/// alpine:3.22, built FROM scratch by the reference, hence its WorkingDir.
fn alpine() -> Config {
    let mut c = image(
        "/bin/sh",
        &[
            (
                "ADD alpine-minirootfs-3.22.6-x86_64.tar.gz /",
                "buildkit.dockerfile.v0",
                false,
            ),
            ("CMD [\"/bin/sh\"]", "buildkit.dockerfile.v0", true),
        ],
    );
    c.config.working_dir = "/".to_string();
    c.config.args_escaped = true;
    c
}

fn options(args: &[(&str, &str)]) -> Options {
    Options {
        build_args: args
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        ..Options::default()
    }
}

/// Every stage of the target's order dispatched, images taken as busybox (or alpine
/// when the name says so).
fn build_with(
    dockerfile: &str,
    options: &Options,
) -> Result<(Prepared, HashMap<usize, Dispatched>)> {
    let prepared = prepare(dockerfile.as_bytes(), options, &amd64())?;
    let mut done: HashMap<usize, Dispatched> = HashMap::new();
    for &s in &prepared.order.clone() {
        let base = match &prepared.stages[s].base {
            BaseRef::Image(n) if n.contains("alpine") => alpine(),
            BaseRef::Image(_) => busybox(),
            BaseRef::Stage(k) => done[k].config.clone(),
            BaseRef::Scratch => Config::scratch(),
        };
        let d = dispatch(&prepared, s, &base, options, &amd64())?;
        done.insert(s, d);
    }
    Ok((prepared, done))
}

fn build(dockerfile: &str) -> (Prepared, HashMap<usize, Dispatched>) {
    build_with(dockerfile, &Options::default()).unwrap()
}

/// The target's dispatch alone.
fn target(dockerfile: &str) -> Dispatched {
    let (prepared, mut done) = build(dockerfile);
    done.remove(&prepared.target).unwrap()
}

fn error(dockerfile: &str) -> String {
    error_with(dockerfile, &Options::default())
}

fn error_with(dockerfile: &str, options: &Options) -> String {
    match build_with(dockerfile, options) {
        Ok(_) => panic!("no error for {dockerfile}"),
        Err(e) => e.to_string(),
    }
}

fn history(d: &Dispatched) -> Vec<&str> {
    d.steps.iter().map(|s| s.created_by.as_str()).collect()
}

fn runs(d: &Dispatched) -> Vec<&Run> {
    d.steps
        .iter()
        .filter_map(|s| match &s.action {
            Some(Action::Run(r)) => Some(r),
            _ => None,
        })
        .collect()
}

fn copies(d: &Dispatched) -> Vec<&Copy> {
    d.steps
        .iter()
        .filter_map(|s| match &s.action {
            Some(Action::Copy(c)) => Some(c),
            _ => None,
        })
        .collect()
}

fn cases() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/build")
}

/// The build arguments of a case's args file.
fn case_options(name: &str) -> Options {
    let mut out = Options::default();
    let Ok(text) = std::fs::read_to_string(cases().join("cases").join(name).join("args")) else {
        return out;
    };
    let words: Vec<&str> = text.split_whitespace().collect();
    for pair in words.windows(2) {
        if pair[0] == "--build-arg" {
            let (k, v) = pair[1].split_once('=').unwrap();
            out.build_args.insert(k.to_string(), v.to_string());
        }
    }
    out
}

fn build_case(name: &str) -> (Prepared, HashMap<usize, Dispatched>) {
    let dockerfile =
        std::fs::read_to_string(cases().join("cases").join(name).join("Dockerfile")).unwrap();
    build_with(&dockerfile, &case_options(name)).unwrap()
}

/// The image a case's target makes, described as tests/build/describe.py describes it
/// (base history first, the builder's name left out).
fn describe(base: &Config, d: &Dispatched) -> Value {
    const KEYS: [&str; 13] = [
        "User",
        "ExposedPorts",
        "Env",
        "Entrypoint",
        "Cmd",
        "Volumes",
        "WorkingDir",
        "Labels",
        "StopSignal",
        "Shell",
        "Healthcheck",
        "OnBuild",
        "ArgsEscaped",
    ];
    let entry = |created_by: &str, comment: &str, empty: bool| {
        let mut item = json!({ "created_by": created_by });
        if empty {
            item["empty_layer"] = json!(true);
        }
        let comment = if comment.ends_with(".dockerfile.v0") {
            "dockerfile.v0"
        } else {
            comment
        };
        if !comment.is_empty() {
            item["comment"] = json!(comment);
        }
        item
    };
    let mut history: Vec<Value> = base
        .history
        .iter()
        .map(|h| entry(&h.created_by, &h.comment, h.empty_layer))
        .collect();
    history.extend(
        d.steps
            .iter()
            .map(|s| entry(&s.created_by, "nspawn.dockerfile.v0", s.action.is_none())),
    );
    let settings = serde_json::to_value(&d.config.config).unwrap();
    let mut config = serde_json::Map::new();
    for k in KEYS {
        match settings.get(k) {
            None | Some(Value::Null) | Some(Value::Bool(false)) => {}
            Some(Value::String(s)) if s.is_empty() => {}
            Some(Value::Array(a)) if a.is_empty() => {}
            Some(Value::Object(o)) if o.is_empty() => {}
            Some(v) => {
                config.insert(k.to_string(), v.clone());
            }
        }
    }
    json!({
        "architecture": d.config.architecture,
        "os": d.config.os,
        "config": config,
        "history": history,
    })
}

fn check_golden(name: &str) {
    let (prepared, done) = build_case(name);
    let golden: Value = serde_json::from_str(
        &std::fs::read_to_string(cases().join("golden").join(format!("{name}.json"))).unwrap(),
    )
    .unwrap();
    let base = match &prepared.stages[prepared.target].base {
        BaseRef::Image(n) if n.contains("alpine") => alpine(),
        BaseRef::Image(_) => busybox(),
        other => panic!("{name}: the target starts from {other:?}"),
    };
    let actual = describe(&base, &done[&prepared.target]);
    for key in ["architecture", "os", "config", "history"] {
        assert_eq!(actual[key], golden[key], "{name}: {key}");
    }
}

#[test]
fn golden_add() {
    check_golden("add");
}

#[test]
fn golden_copy_rules() {
    check_golden("copy-rules");
}

#[test]
fn golden_expansion() {
    check_golden("expansion");
}

#[test]
fn golden_layers() {
    check_golden("layers");
}

#[test]
fn golden_mounts() {
    check_golden("mounts");
}

#[test]
fn golden_multistage() {
    check_golden("multistage");
}

#[test]
fn golden_settings() {
    check_golden("settings");
}

#[test]
fn golden_user_workdir() {
    check_golden("user-workdir");
}

#[test]
fn run_fails_case() {
    let (prepared, done) = build_case("run-fails");
    let d = &done[&prepared.target];
    assert_eq!(
        history(d),
        ["RUN /bin/sh -c echo before", "RUN /bin/sh -c false"]
    );
    // The error the reference reports names the process by its argv.
    assert_eq!(runs(d)[1].argv, ["/bin/sh", "-c", "false"]);
}

#[test]
fn case_add_steps() {
    let (prepared, done) = build_case("add");
    let c = copies(&done[&prepared.target]);
    assert!(c
        .iter()
        .all(|c| c.add && c.from == From::Context && c.unpack.is_none()));
    assert_eq!(c[0].sources, ["tree.tar.gz"]);
    assert_eq!(c[0].dest, "/unpacked/");
    assert_eq!(c[1].sources, ["tree.tar.bz2"]);
    assert_eq!(c[2].sources, ["tree.tar.xz"]);
    assert_eq!(c[4].chown.as_deref(), Some("7:8"));
    assert_eq!(c[4].chmod.as_deref(), Some("0600"));
    assert_eq!(c[4].dest, "/owned.txt");
}

#[test]
fn case_copy_rules_steps() {
    let (prepared, done) = build_case("copy-rules");
    let c = copies(&done[&prepared.target]);
    assert_eq!(c[0].dest, "/dir-as-is");
    assert_eq!(c[1].sources, ["src/"]);
    assert_eq!(c[1].dest, "/dir-slash/");
    assert_eq!(c[5].sources, ["src/a/b/f.txt", "src/c/g.txt"]);
    assert!(c[6].parents);
    assert_eq!(c[7].exclude, ["*.log"]);
    assert!(c.iter().all(|c| !c.add && c.user.is_empty()));
}

#[test]
fn case_layers_steps() {
    let (prepared, done) = build_case("layers");
    let d = &done[&prepared.target];
    assert_eq!(
        d.steps[0].action,
        Some(Action::Workdir {
            path: "/app".to_string(),
            user: String::new()
        })
    );
    let c = copies(d);
    assert_eq!(c[0].dest, "/app/");
    assert_eq!(c[1].chmod.as_deref(), Some("0750"));
    assert_eq!(c[1].dest, "/opt/bin/");
    assert_eq!(c[2].chown.as_deref(), Some("1000:1000"));
    // HOSTNAME is not set, so the unquoted heredoc expands it to nothing: the golden
    // file's digest is that of these bytes.
    assert_eq!(c[3].sources, Vec::<String>::new());
    assert_eq!(c[3].inline.len(), 1);
    assert_eq!(c[3].inline[0].name, "EOT");
    assert_eq!(
        crate::layout::sha256_digest(c[3].inline[0].data.as_bytes()),
        "sha256:5f4c49229ad3c148a35f0af9f7cd671ba71575dd7998041478ab5cd48c7822d4"
    );
    let r = runs(d);
    assert_eq!(r.len(), 5);
    assert_eq!(r[0].workdir, "/app");
    assert_eq!(r[0].env, [PATH_ENV]);
    assert_eq!(
        r[4].argv,
        [
            "/bin/sh",
            "-c",
            "set -e\necho heredoc > /app/heredoc\nmkfifo /app/fifo\n"
        ]
    );
    assert_eq!(r[4].script, None);
    assert_eq!(d.steps[8].description, "RUN <<EOT (set -e...)");
    assert_eq!(d.steps[9].description, "COPY <<EOT /app/inline.txt");
}

#[test]
fn case_mounts_steps() {
    let (prepared, done) = build_case("mounts");
    let r = runs(&done[&prepared.target]);
    assert_eq!(
        r[0].mounts,
        [RunMount::Cache {
            target: "/cache".to_string(),
            id: "/cache".to_string(),
            from: None,
            sharing: "shared".to_string(),
            read_only: false,
            mode: 0o755,
            uid: 0,
            gid: 0,
        }]
    );
    assert_eq!(
        r[1].mounts,
        [RunMount::Bind {
            target: "/src".to_string(),
            from: From::Context,
            source: "/files".to_string(),
            read_write: false,
        }]
    );
    assert_eq!(
        r[2].mounts,
        [RunMount::Tmpfs {
            target: "/scratch".to_string(),
            size: None,
        }]
    );
    assert_eq!(
        r[3].mounts,
        [RunMount::Secret {
            id: "token".to_string(),
            target: Some("/run/secrets/token".to_string()),
            env: None,
            required: false,
            mode: 0o400,
            uid: 0,
            gid: 0,
        }]
    );
    assert_eq!(
        r[4].mounts,
        [RunMount::Secret {
            id: "token".to_string(),
            target: None,
            env: Some("TOKEN".to_string()),
            required: false,
            mode: 0o400,
            uid: 0,
            gid: 0,
        }]
    );
    assert!(r
        .iter()
        .all(|r| r.network == Network::Default && !r.insecure));
}

#[test]
fn case_multistage_order() {
    let (prepared, done) = build_case("multistage");
    assert_eq!(prepared.order, [0, 2]);
    assert_eq!(prepared.target, 2);
    assert_eq!(prepared.stages[0].name, "build");
    assert_eq!(prepared.stages[1].base, BaseRef::Stage(0));
    assert!(
        matches!(&prepared.stages[2].base, BaseRef::Image(n) if n.starts_with("docker.io/library/alpine:3.22@sha256:"))
    );
    let c = copies(&done[&2]);
    assert_eq!(c[0].from, From::Stage(0));
    assert_eq!(c[1].from, From::Stage(0));
    assert_eq!(c[1].dest, "/copied/");
    assert_eq!(
        done[&2].steps[0].description,
        "COPY --from=build /out/artifact /artifact"
    );
}

#[test]
fn case_settings_steps() {
    let (prepared, done) = build_case("settings");
    let d = &done[&prepared.target];
    assert_eq!(
        d.config.config.healthcheck,
        Some(Healthcheck {
            test: strings(&["CMD-SHELL", "test -f /etc/passwd"]),
            interval: 5_000_000_000,
            timeout: 3_000_000_000,
            start_period: 1_000_000_000,
            start_interval: 0,
            retries: 4,
        })
    );
    assert_eq!(d.steps[4].description, "ENV VERSION=2.0");
}

#[test]
fn case_user_workdir_steps() {
    let (prepared, done) = build_case("user-workdir");
    let d = &done[&prepared.target];
    assert_eq!(
        d.steps[2].action,
        Some(Action::Workdir {
            path: "/home/app/work".to_string(),
            user: "app".to_string()
        })
    );
    let r = runs(d);
    assert_eq!(r[1].user, "app");
    assert_eq!(r[1].workdir, "/home/app/work");
    assert_eq!(r[2].user, "root:root");
    assert_eq!(r[2].workdir, "/home/app/work/sub");
}

// convert_test.go

#[test]
fn dockerfile_parsing() {
    build("FROM scratch\nENV FOO bar\nCOPY f1 f2 /sub/\nRUN ls -l\n");
    let df =
        "FROM scratch AS foo\nENV FOO bar\nFROM foo\nCOPY --from=foo f1 /\nCOPY --from=0 f2 /\n\t";
    build(df);
    let mut o = Options {
        target: Some("Foo".to_string()),
        ..Options::default()
    };
    let (prepared, _) = build_with(df, &o).unwrap();
    assert_eq!(prepared.target, 0);
    assert_eq!(prepared.order, [0]);
    o.target = Some("nosuch".to_string());
    assert_eq!(
        error_with(df, &o),
        "target stage \"nosuch\" could not be found"
    );
    build("FROM scratch\n\tADD http://github.com/moby/buildkit/blob/master/README.md /\n\t\t");
    assert_eq!(
        error("FROM scratch\n\tCOPY http://github.com/moby/buildkit/blob/master/README.md /\n\t\t"),
        "source can't be a URL for COPY"
    );
    assert_eq!(
        error("FROM \"\" AS foo"),
        "base name (\"\") should not be blank"
    );
    assert_eq!(
        error("FROM ${BLANK} AS foo"),
        "base name (${BLANK}) should not be blank"
    );
}

#[test]
fn copy_from_keeps_stage_labels() {
    let d = target(
        "FROM scratch AS base\nLABEL marker=base\n\nFROM base AS sibling\nLABEL marker=sibling\n\nFROM base AS consumer\nCOPY --from=sibling / /\n",
    );
    assert_eq!(d.config.config.labels["marker"], "base");
}

#[test]
fn proxy_env_deterministic_order() {
    let args: BTreeMap<String, String> = [
        ("ALL_PROXY", "all-upper"),
        ("all_proxy", "all-lower"),
        ("HTTP_PROXY", "http-upper"),
        ("http_proxy", "http-lower"),
        ("NO_PROXY", "no-proxy"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    assert_eq!(
        proxy_env(&args),
        [
            "HTTP_PROXY=http-lower",
            "http_proxy=http-lower",
            "NO_PROXY=no-proxy",
            "no_proxy=no-proxy",
            "ALL_PROXY=all-lower",
            "all_proxy=all-lower",
        ]
    );
    assert!(proxy_env(&[("FOO".to_string(), "bar".to_string())].into()).is_empty());
}

#[test]
fn proxy_args_reach_run_but_not_history() {
    let o = options(&[("HTTPS_PROXY", "http://proxy:3128")]);
    let (prepared, done) = build_with("FROM busybox\nRUN true\n", &o).unwrap();
    let d = &done[&prepared.target];
    assert_eq!(history(d), ["RUN /bin/sh -c true"]);
    assert_eq!(
        runs(d)[0].env,
        [
            PATH_ENV,
            "HTTPS_PROXY=http://proxy:3128",
            "https_proxy=http://proxy:3128"
        ]
    );
}

#[test]
fn circular_dependencies() {
    assert_eq!(
        error("FROM busybox AS stage0\nCOPY --from=stage0 f1 /sub/\n"),
        "circular dependency detected on stage: stage0"
    );
    assert_eq!(
        error("FROM busybox AS stage0\nCOPY --from=stage2 f1 /sub/\nFROM busybox AS stage1\nCOPY --from=stage0 f2 /sub/\nFROM busybox AS stage2\nCOPY --from=stage1 f2 /sub/\n"),
        "circular dependency detected on stage: stage0"
    );
}

#[test]
fn healthcheck_history() {
    let d = target(
        "FROM busybox\nHEALTHCHECK --interval=1s --timeout=10s --start-period=3s --start-interval=100ms --retries=5 CMD [\"bin\", \"-c\", \"exit 0\"]\n",
    );
    assert_eq!(
        history(&d),
        ["HEALTHCHECK {Test:[CMD bin -c exit 0] Interval:1s Timeout:10s StartPeriod:3s StartInterval:100ms Retries:5}"]
    );
    let d = target("FROM busybox\nHEALTHCHECK NONE\n");
    assert_eq!(
        history(&d),
        ["HEALTHCHECK {Test:[NONE] Interval:0s Timeout:0s StartPeriod:0s StartInterval:0s Retries:0}"]
    );
}

#[test]
fn exclude_patterns() {
    let d = target("FROM scratch\nCOPY --exclude=src/*.go --exclude=tmp/*.txt dir /sub/\n");
    assert_eq!(copies(&d)[0].exclude, ["src/*.go", "tmp/*.txt"]);
    let d = target("FROM scratch\nADD --exclude=src/*.go --exclude=tmp/*.txt dir /sub/\n");
    assert_eq!(copies(&d)[0].exclude, ["src/*.go", "tmp/*.txt"]);
    assert_eq!(history(&d), ["ADD dir /sub/"]);
}

// convert_secrets_test.go, the Linux cases.

#[test]
fn secret_targets() {
    let m = crate::build::dockerfile::instructions::Mount {
        mount_type: crate::build::dockerfile::instructions::MountType::Secret,
        cache_id: "mysecret".to_string(),
        target: "/a\\b".to_string(),
        ..Default::default()
    };
    assert!(
        matches!(stage::secret(&m), Ok(RunMount::Secret { target: Some(t), .. }) if t == "/a\\b")
    );
    let d = target("FROM busybox\nRUN --mount=type=secret,id=mysecret true\n");
    assert!(
        matches!(&runs(&d)[0].mounts[0], RunMount::Secret { target: Some(t), .. } if t == "/run/secrets/mysecret")
    );
    let d = target(
        "FROM busybox\nRUN --mount=type=secret,target=/etc/key,mode=0440,uid=5,required true\n",
    );
    assert_eq!(
        runs(&d)[0].mounts[0],
        RunMount::Secret {
            id: "key".to_string(),
            target: Some("/etc/key".to_string()),
            env: None,
            required: true,
            mode: 0o440,
            uid: 5,
            gid: 0,
        }
    );
    let d = target("FROM busybox\nRUN --mount=type=secret,id=tok,env=TOK echo \"$TOK\" $TOK\n");
    assert_eq!(
        d.steps[0].description,
        "RUN --mount=type=secret,id=tok,env=TOK echo \"****\" ****"
    );
    assert_eq!(history(&d), ["RUN /bin/sh -c echo \"$TOK\" $TOK"]);
}

// dockerfile_history_test.go

#[test]
fn exported_history() {
    let (prepared, done) = build(
        "\nFROM busybox AS base\nENV foo=bar\nCOPY foo /foo2\nFROM busybox\nLABEL lbl=val\nCOPY --from=base foo2 foo3\nWORKDIR /\nRUN echo bar > foo4\nRUN [\"ls\"]\nEXPOSE 2375 5000 1234/udp\n",
    );
    let d = &done[&prepared.target];
    assert_eq!(
        history(d),
        [
            "LABEL lbl=val",
            "COPY foo2 foo3",
            "WORKDIR /",
            "RUN /bin/sh -c echo bar > foo4",
            "RUN ls",
            "EXPOSE [1234/udp 2375/tcp 5000/tcp]",
        ]
    );
    let layers: Vec<bool> = d.steps.iter().map(|s| s.action.is_some()).collect();
    assert_eq!(layers, [false, true, false, true, true, false]);
    assert_eq!(history(&done[&0]), ["ENV foo=bar", "COPY foo /foo2"]);
}

#[test]
fn exported_history_flatten_args() {
    let d = target("\nFROM busybox\nARG foo=bar\nARG bar=123\nARG foo=bar2\nRUN ls /etc/\n");
    assert_eq!(
        history(&d),
        [
            "ARG foo=bar",
            "ARG bar=123",
            "ARG foo=bar2",
            "RUN |2 foo=bar2 bar=123 /bin/sh -c ls /etc/",
        ]
    );
}

// dockerfile_args_test.go

#[test]
fn global_arg() {
    let (prepared, _) = build_with(
        "\nARG tag=nosuchtag\nFROM busybox:${tag}\n",
        &options(&[("tag", "latest")]),
    )
    .unwrap();
    assert_eq!(
        prepared.stages[0].base,
        BaseRef::Image("busybox:latest".to_string())
    );
    assert_eq!(
        prepared.meta_args,
        [("tag".to_string(), Some("latest".to_string()))]
    );
}

#[test]
fn global_arg_errors() {
    let df = "\nARG FOO=${FOO:?\"custom error\"}\nFROM busybox\n";
    assert!(error(df).contains("FOO: custom error"), "{}", error(df));
    build_with(df, &options(&[("FOO", "bar")])).unwrap();
}

#[test]
fn arg_default_expansion() {
    let df = "\nFROM scratch\nARG FOO\nARG BAR=${FOO:?\"foo missing\"}\n";
    assert!(error(df).contains("FOO: foo missing"), "{}", error(df));
    build_with(df, &options(&[("FOO", "123")])).unwrap();
    build_with(df, &options(&[("BAR", "123")])).unwrap();
}

#[test]
fn target_stage_name_arg() {
    let df = "\nFROM alpine AS base\nWORKDIR /out\nRUN echo -n \"value:$TARGETSTAGE\" > /out/first\nARG TARGETSTAGE\nRUN echo -n \"value:$TARGETSTAGE\" > /out/second\nFROM scratch AS foo\nCOPY --from=base /out/ /\nFROM scratch\nCOPY --from=base /out/ /\n";
    let o = Options {
        target: Some("foo".to_string()),
        ..Options::default()
    };
    let (_, done) = build_with(df, &o).unwrap();
    assert_eq!(
        history(&done[&0])[3],
        "RUN |1 TARGETSTAGE=foo /bin/sh -c echo -n \"value:$TARGETSTAGE\" > /out/second"
    );
    assert!(!runs(&done[&0])[0]
        .env
        .iter()
        .any(|e| e.starts_with("TARGETSTAGE")));
    assert!(runs(&done[&0])[1]
        .env
        .contains(&"TARGETSTAGE=foo".to_string()));
    let (_, done) = build(df);
    assert_eq!(
        history(&done[&0])[3],
        "RUN |1 TARGETSTAGE=default /bin/sh -c echo -n \"value:$TARGETSTAGE\" > /out/second"
    );
    let named = format!("{df}\tFROM scratch AS final\n\tCOPY --from=base /out/ /\n");
    let (_, done) = build(&named);
    assert!(history(&done[&0])[3].starts_with("RUN |1 TARGETSTAGE=final "));
}

#[test]
fn builtin_args() {
    let d = target(
        "FROM busybox\nARG TARGETPLATFORM TARGETOS TARGETARCH TARGETVARIANT BUILDPLATFORM\nRUN true\n",
    );
    assert_eq!(
        history(&d),
        [
            "ARG TARGETPLATFORM=linux/amd64 TARGETOS=linux TARGETARCH=amd64 TARGETVARIANT= BUILDPLATFORM=linux/amd64",
            "RUN |5 TARGETPLATFORM=linux/amd64 TARGETOS=linux TARGETARCH=amd64 TARGETVARIANT= BUILDPLATFORM=linux/amd64 /bin/sh -c true",
        ]
    );
}

#[test]
fn args_of_a_base_stage_carry_over() {
    let (_, done) = build(
        "FROM busybox AS base\nARG VERSION=1\nENV A=x\nFROM base\nARG OTHER=2\nRUN echo $VERSION\n",
    );
    let child = &done[&1];
    assert_eq!(
        history(child),
        [
            "ARG OTHER=2",
            "RUN |2 VERSION=1 OTHER=2 /bin/sh -c echo $VERSION"
        ]
    );
    assert_eq!(
        runs(child)[0].env,
        ["VERSION=1", PATH_ENV, "A=x", "OTHER=2"]
    );
    assert_eq!(child.config.config.env, [PATH_ENV, "A=x"]);
}

#[test]
fn stage_dispatched_before_its_children() {
    let prepared = prepare(
        b"FROM busybox AS a\nFROM a\n",
        &Options::default(),
        &amd64(),
    )
    .unwrap();
    let err = dispatch(&prepared, 1, &busybox(), &Options::default(), &amd64()).unwrap_err();
    assert_eq!(
        err.to_string(),
        "stage 0 must be dispatched before stage 1, which starts from it"
    );
}

// The instructions one by one.

#[test]
fn env_and_expansion() {
    let d = target("FROM busybox\nENV A=1 B=$A\nENV C=\"$A\" D='$A' PATH=$PATH:/x\n");
    assert_eq!(
        history(&d),
        [
            "ENV A=1 B=",
            "ENV C=1 D=$A PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/x"
        ]
    );
    assert_eq!(
        d.config.config.env,
        [
            format!("{PATH_ENV}:/x"),
            "A=1".to_string(),
            "B=".to_string(),
            "C=1".to_string(),
            "D=$A".to_string()
        ]
    );
    let d = target("# escape=`\nFROM busybox\nENV A=1 B=`$A\n");
    assert_eq!(history(&d), ["ENV A=1 B=$A"]);
}

#[test]
fn cmd_and_entrypoint() {
    let d = target("FROM busybox\nENTRYPOINT [\"/bin/app\"]\n");
    assert_eq!(d.config.config.cmd, None);
    assert_eq!(d.config.config.entrypoint, Some(strings(&["/bin/app"])));
    assert!(!d.config.config.args_escaped);
    assert_eq!(history(&d), ["ENTRYPOINT [\"/bin/app\"]"]);

    let d = target("FROM busybox\nCMD [\"a\"]\nENTRYPOINT run me\n");
    assert_eq!(d.config.config.cmd, Some(strings(&["a"])));
    assert_eq!(
        d.config.config.entrypoint,
        Some(strings(&["/bin/sh", "-c", "run me"]))
    );
    assert!(d.config.config.args_escaped);
    assert_eq!(
        history(&d),
        ["CMD [\"a\"]", "ENTRYPOINT [\"/bin/sh\" \"-c\" \"run me\"]"]
    );

    let d = target("FROM busybox\nSHELL [\"/bin/bash\", \"-o\", \"pipefail\", \"-c\"]\nCMD echo \"hi\"\nENTRYPOINT []\n");
    assert_eq!(
        d.config.config.cmd,
        Some(strings(&[
            "/bin/bash",
            "-o",
            "pipefail",
            "-c",
            "echo \"hi\""
        ]))
    );
    assert_eq!(d.config.config.entrypoint, None);
    assert_eq!(
        history(&d),
        [
            "SHELL [/bin/bash -o pipefail -c]",
            "CMD [\"/bin/bash\" \"-o\" \"pipefail\" \"-c\" \"echo \\\"hi\\\"\"]",
            "ENTRYPOINT []",
        ]
    );
    let d = target("FROM busybox\nCMD []\n");
    assert_eq!(d.config.config.cmd, None);
    assert_eq!(history(&d), ["CMD []"]);
}

#[test]
fn labels_of_the_build_go_to_the_target_only() {
    let mut o = Options::default();
    o.labels.insert("org.test".to_string(), "yes".to_string());
    let (_, done) = build_with("FROM busybox AS a\nLABEL a=1\nFROM a\nLABEL b=2\n", &o).unwrap();
    assert_eq!(done[&0].config.config.labels.len(), 1);
    let labels = &done[&1].config.config.labels;
    assert_eq!(labels["a"], "1");
    assert_eq!(labels["b"], "2");
    assert_eq!(labels["org.test"], "yes");
    assert_eq!(history(&done[&1]), ["LABEL b=2"]);
}

#[test]
fn workdirs() {
    let d = target("FROM busybox\nWORKDIR a\nWORKDIR b/../c/\nWORKDIR /x//y/\nWORKDIR /\n");
    assert_eq!(
        history(&d),
        ["WORKDIR /a", "WORKDIR /a/c", "WORKDIR /x//y/", "WORKDIR /"]
    );
    assert!(d.steps[3].action.is_none());
    assert_eq!(d.config.config.working_dir, "/");
    let d = target("FROM busybox\nARG D=dir\nWORKDIR /$D\nUSER 1000\nWORKDIR sub\n");
    assert_eq!(
        d.steps[3].action,
        Some(Action::Workdir {
            path: "/dir/sub".to_string(),
            user: "1000".to_string()
        })
    );
    assert_eq!(d.steps[1].description, "WORKDIR /dir");
    let d = target("FROM scratch\nRUN true\n");
    assert_eq!(d.config.config.working_dir, "/");
    assert_eq!(d.config.config.env, [PATH_ENV]);
}

#[test]
fn volumes_signals_users_maintainer() {
    let d = target("FROM busybox\nVOLUME [\"/a\", \"/b\"]\nSTOPSIGNAL sigterm\nUSER nobody:nogroup\nMAINTAINER someone <a@b.c>\n");
    assert_eq!(
        history(&d),
        [
            "VOLUME [/a /b]",
            "STOPSIGNAL sigterm",
            "USER nobody:nogroup",
            "MAINTAINER someone <a@b.c>",
        ]
    );
    assert_eq!(d.config.config.stop_signal, "sigterm");
    assert_eq!(d.author.as_deref(), Some("someone <a@b.c>"));
    for ok in ["9", "SIGRTMIN+3", "RTMAX-14", "-5", "rtmax"] {
        target(&format!("FROM busybox\nSTOPSIGNAL {ok}\n"));
    }
    for bad in [
        "0", "SIGNOPE", "RTMIN+16", "RTMAX-15", "RTMIN+01", "RTMIN++5", "RTMAX-+2",
    ] {
        assert_eq!(
            error(&format!("FROM busybox\nSTOPSIGNAL {bad}\n")),
            format!("invalid signal: {bad}")
        );
    }
    assert_eq!(
        error("FROM busybox\nARG E\nVOLUME $E\n"),
        "VOLUME specified can not be an empty string"
    );
}

#[test]
fn expose() {
    let d = target("FROM busybox\nARG P=80\nEXPOSE $P/udp 22\n");
    // Sorted as written, then expanded.
    assert_eq!(history(&d)[1], "EXPOSE [80/udp 22/tcp]");
    assert_eq!(
        d.config.config.exposed_ports.keys().collect::<Vec<_>>(),
        ["22/tcp", "80/udp"]
    );
    assert_eq!(
        error("FROM busybox\nEXPOSE 80/icmp\n"),
        "invalid port: \"80/icmp\": invalid proto: icmp"
    );
}

#[test]
fn onbuild_triggers() {
    let mut base = busybox();
    base.config.on_build = strings(&["RUN echo from-image", "ENV T=1"]);
    let df = "FROM busybox\nONBUILD RUN echo later\nRUN echo own\n";
    let prepared = prepare(df.as_bytes(), &Options::default(), &amd64()).unwrap();
    let d = dispatch(&prepared, 0, &base, &Options::default(), &amd64()).unwrap();
    assert_eq!(
        history(&d),
        [
            "RUN /bin/sh -c echo from-image",
            "ENV T=1",
            "RUN /bin/sh -c echo own",
        ]
    );
    assert_eq!(d.steps[0].description, "ONBUILD RUN echo from-image");
    assert_eq!(d.steps[2].description, "RUN echo own");
    assert_eq!(d.config.config.on_build, ["RUN echo later"]);

    // A stage FROM an earlier one runs that one's ONBUILD instructions, which may take
    // from stages nothing else needs.
    let (prepared, done) = build(
        "FROM busybox AS input\nRUN touch /in\nFROM busybox AS base\nONBUILD COPY --from=input /in /in\nONBUILD RUN ls /in\nFROM base\nRUN true\n",
    );
    assert_eq!(prepared.order, [0, 1, 2]);
    assert_eq!(
        history(&done[&2]),
        [
            "COPY /in /in",
            "RUN /bin/sh -c ls /in",
            "RUN /bin/sh -c true"
        ]
    );
    assert_eq!(copies(&done[&2])[0].from, From::Stage(0));
    assert!(done[&2].config.config.on_build.is_empty());

    for (trigger, message) in [
        (
            "ONBUILD ONBUILD RUN x",
            "Chaining ONBUILD via `ONBUILD ONBUILD` isn't allowed",
        ),
        ("FROM alpine", "*instructions.Stage is not a command type"),
        (
            "RUN a\nRUN b",
            "onbuild trigger should be a single expression",
        ),
    ] {
        let mut base = busybox();
        base.config.on_build = vec![trigger.to_string()];
        let prepared = prepare(b"FROM busybox\n", &Options::default(), &amd64()).unwrap();
        let err = dispatch(&prepared, 0, &base, &Options::default(), &amd64()).unwrap_err();
        assert_eq!(err.to_string(), message, "{trigger}");
    }
    let mut base = busybox();
    base.config.on_build = strings(&["ONBUILD RUN x"]);
    let prepared = prepare(b"FROM busybox\n", &Options::default(), &amd64()).unwrap();
    let d = dispatch(&prepared, 0, &base, &Options::default(), &amd64()).unwrap();
    assert!(d.steps.is_empty());
    assert_eq!(d.config.config.on_build, ["RUN x"]);
}

#[test]
fn run_heredocs() {
    let d = target("FROM busybox\nRUN <<EOF\n#!/bin/sh\necho hi\nEOF\n");
    let r = runs(&d)[0];
    assert_eq!(r.argv, ["/bin/sh", "-c", "/dev/pipes/EOF"]);
    assert_eq!(
        r.script,
        Some(Inline {
            name: "EOF".to_string(),
            data: "#!/bin/sh\necho hi\n".to_string()
        })
    );
    assert_eq!(history(&d), ["RUN /bin/sh -c /dev/pipes/EOF"]);
    assert_eq!(d.steps[0].description, "RUN <<EOF (#!/bin/sh...)");

    let d = target("FROM busybox\nRUN <<-EOF\n\techo a\n\tEOF\n");
    assert_eq!(runs(&d)[0].argv, ["/bin/sh", "-c", "echo a\n"]);

    let d = target("FROM busybox\nRUN cat <<A > /a && cat <<B > /b\none\nA\ntwo\nB\n");
    assert_eq!(
        runs(&d)[0].argv,
        [
            "/bin/sh",
            "-c",
            "cat <<A > /a && cat <<B > /b\none\nA\ntwo\nB"
        ]
    );
}

#[test]
fn run_flags() {
    let d = target(
        "FROM busybox\nRUN --network=none --security=insecure true\nRUN --network=host true\n",
    );
    let r = runs(&d);
    assert_eq!(r[0].network, Network::None);
    assert!(r[0].insecure);
    assert_eq!(r[1].network, Network::Host);
    assert_eq!(history(&d), ["RUN /bin/sh -c true", "RUN /bin/sh -c true"]);
    assert_eq!(
        error("FROM busybox\nRUN --mount=type=ssh true\n"),
        "RUN --mount=type=ssh is not supported: nspawn's builder does not forward an SSH agent"
    );
    assert_eq!(
        error("FROM busybox\nRUN --mount=type=bind,target=/ true\n"),
        "invalid mount target \"/\""
    );
}

#[test]
fn run_mounts() {
    let d = target(
        "FROM busybox AS src\nFROM busybox\nWORKDIR /w\nARG ID=c1\nRUN --mount=type=cache,target=rel,id=$ID,sharing=locked,from=src,source=/s,uid=7,mode=0700 --mount=type=bind,from=alpine,target=/a,rw --mount=type=tmpfs,target=/t,size=1k --mount=type=bind,from=scratch,target=/e true\n",
    );
    assert_eq!(
        runs(&d)[0].mounts,
        [
            RunMount::Cache {
                target: "/w/rel".to_string(),
                id: "c1".to_string(),
                from: Some((From::Stage(0), "/s".to_string())),
                sharing: "locked".to_string(),
                read_only: false,
                mode: 0o700,
                uid: 7,
                gid: 0,
            },
            RunMount::Bind {
                target: "/a".to_string(),
                from: From::Image("alpine".to_string()),
                source: "/".to_string(),
                read_write: true,
            },
            RunMount::Tmpfs {
                target: "/t".to_string(),
                size: Some(1024),
            },
            RunMount::Bind {
                target: "/e".to_string(),
                from: From::Scratch,
                source: "/".to_string(),
                read_write: false,
            },
        ]
    );
    let d = target("FROM busybox\nRUN --mount=type=cache,target=/var/cache/../apt true\n");
    assert!(matches!(&runs(&d)[0].mounts[0], RunMount::Cache { id, .. } if id == "/var/apt"));
}

#[test]
fn copy_and_add_rules() {
    let d = target("FROM busybox\nARG U=1:2\nCOPY --chown=$U --chmod=u+x --link a \"b\" /d\n");
    let c = copies(&d)[0];
    assert_eq!(c.chown.as_deref(), Some("1:2"));
    assert_eq!(c.chmod.as_deref(), Some("u+x"));
    // --chmod takes the --link away.
    assert!(!c.link);
    assert_eq!(c.sources, ["a", "b"]);
    assert_eq!(history(&d)[1], "COPY --chown=1:2 --chmod=u+x a b /d");
    assert_eq!(
        d.steps[1].description,
        "COPY --chown=1:2 --chmod=u+x --link a b /d"
    );

    let sum = format!("sha256:{}", "a1".repeat(32));
    let d = target(&format!(
        "FROM busybox\nADD --checksum={sum} --unpack=true https://example.com/a.tar.gz?x=1 /\n"
    ));
    let c = copies(&d)[0];
    assert_eq!(c.unpack, Some(true));
    assert_eq!(c.checksum.as_deref(), Some(sum.as_str()));
    assert_eq!(url_file_name(&c.sources[0]), "a.tar.gz");

    for (df, message) in [
        ("COPY --chmod=7778 a /", "invalid chmod parameter: '7778'. it should be octal string and between 0 and 07777"),
        ("COPY --chmod=10000 a /", "invalid chmod parameter: '10000'. it should be octal string and between 0 and 07777"),
        ("COPY --chmod=u+q a /", "invalid syntax"),
        ("ADD --checksum=sha256:abc a b /", "checksum can't be specified for multiple sources"),
        ("ADD --checksum=sha256:abc a /", "checksum requires HTTP(S) or Git sources"),
        ("ADD --checksum=sha256:abc https://example.com/a /", "invalid checksum digest length"),
        ("COPY https://github.com/moby/buildkit.git /", "source can't be a git ref for COPY"),
        ("COPY git@github.com:moby/buildkit.git /", "source can't be a git ref for COPY"),
        ("ADD https://github.com/moby/buildkit.git#v0.33.1 /", "ADD of the git repository \"https://github.com/moby/buildkit.git#v0.33.1\" is not supported: nspawn's builder does not fetch git sources"),
        ("COPY --from=$X a /", "variable expansion is not supported for --from, define a new stage with FROM using ARG from global scope as a workaround"),
        ("COPY --from=3 a /", "invalid stage index 3"),
        ("COPY --from=-1 a /", "invalid stage index -1"),
        ("COPY --from=Alpine a /", "failed to parse stage name \"Alpine\": invalid reference format: repository name (library/Alpine) must be lowercase"),
    ] {
        assert_eq!(error(&format!("FROM busybox\n{df}\n")), message, "{df}");
    }
    // A github.com/ path is a path of the context.
    let d = target("FROM busybox\nADD github.com/moby/buildkit /src\n");
    assert_eq!(copies(&d)[0].sources, ["github.com/moby/buildkit"]);
    let d = target(
        "FROM busybox\nCOPY --from=alpine:3.22 /etc/os-release /\nCOPY --from=scratch /x /\n",
    );
    assert_eq!(copies(&d)[0].from, From::Image("alpine:3.22".to_string()));
    assert_eq!(copies(&d)[1].from, From::Scratch);
}

#[test]
fn copy_destinations() {
    let d = target("FROM busybox\nWORKDIR /w\nCOPY a .\nCOPY a b/\nCOPY a /abs\nCOPY a ./c/.\n");
    let dests: Vec<&str> = copies(&d).iter().map(|c| c.dest.as_str()).collect();
    assert_eq!(dests, ["/w/", "/w/b/", "/abs", "/w/c/."]);
}

#[test]
fn later_stages() {
    assert_eq!(
        error("FROM busybox AS a\nCOPY --from=b /x /\nFROM busybox AS b\nFROM a\n"),
        "cannot copy from stage \"b\", it needs to be defined before current stage \"a\""
    );
    assert_eq!(
        error("FROM busybox AS first\nRUN --mount=from=later,target=/l true\nFROM busybox AS later\nFROM first\n"),
        "cannot mount from stage \"later\" to \"/l\", stage needs to be defined before current command"
    );
    // FROM names an earlier stage only; a later one is an image.
    let (prepared, _) = build("FROM late AS early\nFROM busybox AS late\n");
    assert_eq!(prepared.stages[0].base, BaseRef::Image("late".to_string()));
    assert_eq!(prepared.order, [1]);
}

#[test]
fn bases() {
    assert_eq!(
        error("FROM Busybox\n"),
        "failed to parse stage name \"Busybox\": invalid reference format: repository name (library/Busybox) must be lowercase"
    );
    // Unreachable stages are checked as well.
    assert!(error("FROM bad//name AS x\nFROM busybox\n").starts_with("failed to parse stage name"));
    // A digest the store does not keep images by is refused before anything is built.
    let sha512 = format!("busybox@sha512:{}", "a".repeat(128));
    assert_eq!(
        error(&format!("FROM {sha512}\n")),
        format!("\"{sha512}\": an image is taken by a sha256 digest only")
    );
    assert_eq!(
        error(&format!("FROM busybox\nCOPY --from={sha512} /x /x\n")),
        format!("\"{sha512}\": an image is taken by a sha256 digest only")
    );
    // Only what the target builds is fetched: a stage it does not need is parsed alone.
    let (prepared, _) = build(&format!(
        "FROM {sha512} AS legacy\nFROM busybox AS old\nCOPY --from={sha512} /x /x\nFROM busybox\nRUN true\n"
    ));
    assert_eq!(prepared.order, [2]);
    let (prepared, _) = build(&format!("FROM busybox@sha256:{}\n", "a".repeat(64)));
    assert_eq!(prepared.order, [0]);
    let (prepared, _) = build("ARG V=1.37\nFROM busybox:$V AS b\nFROM b\n");
    assert_eq!(
        prepared.stages[0].base,
        BaseRef::Image("busybox:1.37".to_string())
    );
    assert_eq!(prepared.stages[1].base, BaseRef::Stage(0));
    assert_eq!(prepared.order, [0, 1]);
    assert_eq!(error(""), "the Dockerfile cannot be empty");
    assert_eq!(error("ARG A=1\n"), "dockerfile contains no stages to build");
}

#[test]
fn target_suggestions() {
    let o = Options {
        target: Some("buidl".to_string()),
        ..Options::default()
    };
    assert_eq!(
        error_with("FROM busybox AS build\nFROM busybox\n", &o),
        "target stage \"buidl\" could not be found (did you mean build?)"
    );
}

#[test]
fn from_platform() {
    let (prepared, _) = build("FROM --platform=$BUILDPLATFORM busybox AS b\nFROM --platform=linux/x86_64 busybox\nCOPY --from=b / /\n");
    assert_eq!(prepared.order, [0, 1]);
    assert_eq!(
        error("FROM --platform=linux/arm64 busybox\n"),
        "FROM --platform=linux/arm64 asks for linux/arm64: nspawn builds only for the platform of this host, linux/amd64"
    );
    // A stage the target does not need may be for any platform, and the platform of a
    // stage FROM scratch or FROM another stage is not the one asked for.
    build("FROM --platform=linux/arm64 busybox AS other\nFROM busybox\n");
    build("FROM --platform=linux/arm64 scratch\n");
    build("FROM busybox AS b\nFROM --platform=linux/arm64 b\n");
    assert_eq!(
        error("ARG P\nFROM --platform=$P busybox\n"),
        "empty platform value from expression $P"
    );
    assert_eq!(
        error("FROM --platform=linux/* busybox\n"),
        "failed to parse platform linux/*: \"linux/*\": wildcards not yet supported: invalid argument"
    );
    assert_eq!(
        error("FROM --platform=$TARGETPLATFROM busybox\n"),
        "empty platform value from expression $TARGETPLATFROM (did you mean TARGETPLATFORM?)"
    );
}

#[test]
fn syntax_directive() {
    for ok in [
        "docker/dockerfile:1",
        "docker/dockerfile:1.7",
        "docker/dockerfile:1.27.0@sha256:4edf897a3ffa55b89f906fc8cc78afdb3f1834cc9c7083565e611a8a7d5fe99e",
        "docker.io/docker/dockerfile:1-labs",
        "docker/dockerfile:1.4-labs",
        "docker/dockerfile:labs",
    ] {
        build(&format!("# syntax={ok}\nFROM busybox\n"));
        build(&format!("# SYNTAX = {ok} extra\nFROM busybox\n"));
    }
    for bad in [
        "docker/dockerfile:2",
        "docker/dockerfile",
        "Docker/Dockerfile:1",
        "docker/dockerfile-upstream:1",
        "example.com/my/frontend:1",
        "docker/dockerfile:1@sha256:abc",
    ] {
        assert_eq!(
            error(&format!("# syntax={bad}\nFROM busybox\n")),
            format!("the syntax directive asks for the frontend image \"{bad}\": nspawn builds Dockerfiles with its own port of the reference frontend and accepts only docker/dockerfile:1, :1.N or :1.N.M, their -labs variants and :labs, optionally with an @sha256 digest"),
        );
    }
    // A syntax directive after the first instruction is a comment.
    build("FROM busybox\n# syntax=other/frontend\n");
    assert!(error_with(
        "FROM busybox\n",
        &options(&[("BUILDKIT_SYNTAX", "other/frontend:1")])
    )
    .starts_with(
        "the build argument BUILDKIT_SYNTAX asks for the frontend image \"other/frontend:1\""
    ));
    build_with(
        "# syntax=other/frontend\nFROM busybox\n",
        &options(&[("BUILDKIT_SYNTAX", " docker/dockerfile:1 x")]),
    )
    .unwrap();
}

#[test]
fn url_file_names() {
    assert_eq!(
        url_file_name("https://example.com/a/b.tar.gz?x=1#f"),
        "b.tar.gz"
    );
    assert_eq!(url_file_name("https://example.com/a%20b"), "a b");
    assert_eq!(url_file_name("https://example.com"), "__unnamed__");
    assert_eq!(url_file_name("https://example.com/"), "__unnamed__");
    assert_eq!(url_file_name("https://example.com/%zz"), "__unnamed__");
}

#[test]
fn steps_have_descriptions() {
    let d = target("FROM busybox\nARG X=1\nrun echo \"$X\" $Y\ncopy a /$X/\n");
    let descriptions: Vec<&str> = d.steps.iter().map(|s| s.description.as_str()).collect();
    assert_eq!(descriptions, ["ARG X=1", "RUN echo \"1\" $Y", "COPY a /1/"]);
    assert_eq!(
        d.steps[1],
        Step {
            created_by: "RUN |1 X=1 /bin/sh -c echo \"$X\" $Y".to_string(),
            action: d.steps[1].action.clone(),
            description: "RUN echo \"1\" $Y".to_string(),
        }
    );
}

// dockerfile_image_test.go

#[test]
fn scratch_config() {
    let d = target("\nFROM scratch\nENV foo=bar\n");
    assert_eq!(history(&d), ["ENV foo=bar"]);
    assert!(d.steps[0].action.is_none());
    assert_eq!(d.config.config.env, [PATH_ENV, "foo=bar"]);
    assert_eq!(d.config.config.working_dir, "/");
    assert_eq!(d.config.os, "linux");
    assert_eq!(d.config.architecture, "amd64");
    assert!(d.config.rootfs.diff_ids.is_empty());
    assert_eq!(d.config.rootfs.kind, "layers");
}

#[test]
fn maintainer_carries_over() {
    let (_, done) = build("FROM busybox AS a\nMAINTAINER me\nFROM a\nRUN true\n");
    assert_eq!(done[&1].author.as_deref(), Some("me"));
    let (_, done) = build("FROM busybox\nRUN true\n");
    assert_eq!(done[&0].author, None);
}

#[test]
fn link_with_chmod() {
    let d = target("FROM busybox\nCOPY --link a /a\nCOPY --link --chmod=0644 b /b\n");
    let c = copies(&d);
    assert!(c[0].link);
    assert!(!c[1].link);
}

#[test]
fn base_image_workdir_and_user() {
    let mut base = busybox();
    base.config.working_dir = "rel".to_string();
    base.config.user = "app".to_string();
    let prepared = prepare(
        b"FROM busybox\nRUN pwd\nCOPY a b\n",
        &Options::default(),
        &amd64(),
    )
    .unwrap();
    let d = dispatch(&prepared, 0, &base, &Options::default(), &amd64()).unwrap();
    // A relative WorkingDir of an image is joined to itself, as the reference does.
    assert_eq!(d.config.config.working_dir, "/rel/rel");
    assert_eq!(runs(&d)[0].workdir, "/rel/rel");
    assert_eq!(runs(&d)[0].user, "app");
    assert_eq!(copies(&d)[0].dest, "/rel/rel/b");
    assert_eq!(copies(&d)[0].user, "app");
}

#[test]
fn a_long_chain_of_stages_is_walked_without_the_stack() {
    let n = 200_000;
    let chain: Vec<Vec<usize>> = (0..n)
        .map(|i| if i + 1 < n { vec![i + 1] } else { vec![] })
        .collect();
    assert_eq!(first_cycle(&chain), None);
    let mut cycle = chain;
    cycle[n - 1] = vec![3];
    assert_eq!(first_cycle(&cycle), Some(0));
    assert_eq!(first_cycle(&[vec![], vec![2], vec![1]]), Some(1));
}
