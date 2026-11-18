//! The changes `commit --change` and `import --change` make to an image's settings:
//! Dockerfile instructions of the kinds that set what a container starts with, applied
//! one after another as BuildFromConfig of the reference applies them, with no history
//! of their own.

use anyhow::{anyhow, bail, Result};

use super::stage::{check_signal, normalize_workdir, with_shell};
use super::{expose, EnvList};
use crate::build::dockerfile::instructions::{parse_command, Command};
use crate::build::dockerfile::parser;
use crate::build::dockerfile::shell::Lex;
use crate::build::image::{Config, Healthcheck};

/// The instructions a change may be.
pub const ALLOWED: [&str; 11] = [
    "cmd",
    "entrypoint",
    "healthcheck",
    "env",
    "expose",
    "label",
    "onbuild",
    "stopsignal",
    "user",
    "volume",
    "workdir",
];

/// Applies `changes`, each a Dockerfile instruction, to `config` in order.
pub fn apply(config: &mut Config, changes: &[String]) -> Result<()> {
    if changes.is_empty() {
        return Ok(());
    }
    let parsed = parser::parse(changes.join("\n").as_bytes()).map_err(|e| anyhow!("{e}"))?;
    let mut env = EnvList::default();
    for var in &config.config.env {
        let (key, value) = var.split_once('=').unwrap_or((var, ""));
        env.set(key, value);
    }
    let lex = Lex::new('\\');
    // A CMD of these changes, which a later ENTRYPOINT keeps.
    let mut cmd_set = false;
    for node in &parsed.ast.children {
        let name = node.value.to_lowercase();
        if !ALLOWED.contains(&name.as_str()) {
            bail!(
                "{} is not a valid change command",
                node.value.to_uppercase()
            );
        }
        let mut cmd = parse_command(node).map_err(|e| anyhow!("{e}"))?;
        if let Some(ex) = cmd.as_single_word_expansion() {
            ex.expand(&mut |w: &str| lex.process_word(w, &env).map(|(s, _)| s))
                .map_err(|e| anyhow!("{e}"))?;
        }
        if let Some(ex) = cmd.as_single_word_expansion_raw() {
            let mut raw = Lex::new('\\');
            raw.skip_process_quotes = true;
            ex.expand_raw(&mut |w: &str| raw.process_word(w, &env).map(|(s, _)| s))
                .map_err(|e| anyhow!("{e}"))?;
        }
        let settings = &mut config.config;
        match cmd {
            Command::Env(c) => {
                for e in &c.env {
                    env.set(&e.key, &e.value);
                    settings.set_env(&e.key, &e.value);
                }
            }
            Command::Label(c) => {
                for l in &c.labels {
                    settings.labels.insert(l.key.clone(), l.value.clone());
                }
            }
            Command::Onbuild(c) => settings.on_build.push(c.expression.clone()),
            Command::Cmd(c) => {
                let args = if c.prepend_shell {
                    with_shell(config, &c.cmd_line)
                } else {
                    c.cmd_line.clone()
                };
                config.config.cmd = (!args.is_empty()).then_some(args);
                cmd_set = true;
            }
            Command::Entrypoint(c) => {
                let args = if c.prepend_shell {
                    with_shell(config, &c.cmd_line)
                } else {
                    c.cmd_line.clone()
                };
                config.config.entrypoint = (!args.is_empty()).then_some(args);
                if !cmd_set {
                    config.config.cmd = None;
                }
            }
            Command::Healthcheck(c) => {
                let h = &c.health;
                let nanos =
                    |d: std::time::Duration| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX);
                settings.healthcheck = Some(Healthcheck {
                    test: h.test.clone(),
                    interval: nanos(h.interval),
                    timeout: nanos(h.timeout),
                    start_period: nanos(h.start_period),
                    start_interval: nanos(h.start_interval),
                    retries: i64::from(h.retries),
                });
            }
            Command::Expose(c) => {
                let mut ports = Vec::new();
                for p in &c.ports {
                    ports.extend(lex.process_words(p, &env).map_err(|e| anyhow!("{e}"))?);
                }
                for p in expose::parse_ports(&ports).map_err(|e| anyhow!("{e}"))? {
                    settings.exposed_ports.insert(p, serde_json::json!({}));
                }
            }
            Command::User(c) => settings.user.clone_from(&c.user),
            Command::Volume(c) => {
                for v in &c.volumes {
                    if v.is_empty() {
                        bail!("VOLUME specified can not be an empty string");
                    }
                    settings.volumes.insert(v.clone(), serde_json::json!({}));
                }
            }
            Command::StopSignal(c) => {
                check_signal(&c.signal).map_err(|e| anyhow!("{e}"))?;
                settings.stop_signal.clone_from(&c.signal);
            }
            Command::Workdir(c) => {
                settings.working_dir = normalize_workdir(&settings.working_dir, &c.path);
            }
            other => bail!("{} is not a valid change command", other.code()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn applied(changes: &[&str]) -> Result<Config> {
        let mut config = Config::scratch();
        config.config.env = vec!["PATH=/usr/bin".to_string()];
        config.config.cmd = Some(vec!["sh".to_string()]);
        let changes: Vec<String> = changes.iter().map(|c| c.to_string()).collect();
        apply(&mut config, &changes)?;
        Ok(config)
    }

    #[test]
    fn changes_set_what_a_container_starts_with() {
        let c = applied(&[
            "ENV PATH=$PATH:/opt/bin GREETING=hi",
            "LABEL a=b",
            "EXPOSE 80 53/udp",
            "USER app",
            "WORKDIR /srv",
            "WORKDIR data",
            "VOLUME /data",
            "STOPSIGNAL SIGINT",
            "CMD [\"serve\"]",
        ])
        .unwrap();
        let s = &c.config;
        assert_eq!(s.env, ["PATH=/usr/bin:/opt/bin", "GREETING=hi"]);
        assert_eq!(s.labels.get("a").map(String::as_str), Some("b"));
        assert!(s.exposed_ports.contains_key("80/tcp"));
        assert!(s.exposed_ports.contains_key("53/udp"));
        assert_eq!(s.user, "app");
        assert_eq!(s.working_dir, "/srv/data");
        assert!(s.volumes.contains_key("/data"));
        assert_eq!(s.stop_signal, "SIGINT");
        assert_eq!(s.cmd.as_deref(), Some(&["serve".to_string()][..]));
    }

    #[test]
    fn an_entrypoint_drops_the_command_unless_one_came_before() {
        let c = applied(&["ENTRYPOINT [\"/app\"]"]).unwrap();
        assert_eq!(
            c.config.entrypoint.as_deref(),
            Some(&["/app".to_string()][..])
        );
        assert_eq!(c.config.cmd, None);
        let c = applied(&["CMD [\"-v\"]", "ENTRYPOINT [\"/app\"]"]).unwrap();
        assert_eq!(c.config.cmd.as_deref(), Some(&["-v".to_string()][..]));
        // The shell form runs in /bin/sh -c.
        let c = applied(&["CMD echo hi"]).unwrap();
        assert_eq!(
            c.config.cmd.as_deref(),
            Some(
                &[
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "echo hi".to_string()
                ][..]
            )
        );
    }

    #[test]
    fn only_settings_are_changes() {
        for refused in [
            "RUN true",
            "COPY a b",
            "FROM scratch",
            "ARG x",
            "SHELL [\"sh\"]",
        ] {
            let e = applied(&[refused]).unwrap_err().to_string();
            assert!(
                e.ends_with("is not a valid change command"),
                "{refused}: {e}"
            );
        }
        assert!(applied(&["STOPSIGNAL NOPE"]).is_err());
    }
}
