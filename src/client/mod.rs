//! The command line as a client of org.nspawn: proxies for the service's interfaces,
//! the connection, how the service's errors read here, and how a job is followed.

use std::collections::HashMap;

use anyhow::{anyhow, Context as _, Result};
use futures_util::StreamExt;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use crate::config::Config;

pub type Dict = HashMap<String, OwnedValue>;
pub type Options<'a> = HashMap<&'a str, Value<'a>>;

#[zbus::proxy(
    interface = "org.nspawn.Manager",
    default_service = "org.nspawn",
    default_path = "/org/nspawn",
    gen_blocking = false
)]
pub trait Manager {
    #[zbus(property)]
    fn version(&self) -> zbus::Result<String>;

    fn list_images(&self) -> zbus::Result<Vec<Dict>>;
    fn get_image(&self, name: &str) -> zbus::Result<Dict>;
    fn pull_image(&self, reference: &str, options: Options<'_>) -> zbus::Result<OwnedObjectPath>;
    fn create_machine(
        &self,
        source: &str,
        name: &str,
        options: Options<'_>,
    ) -> zbus::Result<OwnedObjectPath>;
    fn push_image(&self, image: &str, options: Options<'_>) -> zbus::Result<OwnedObjectPath>;
    fn build_image(
        &self,
        directory: &str,
        tag: &str,
        options: Options<'_>,
    ) -> zbus::Result<OwnedObjectPath>;
    fn remove_images(&self, names: &[String]) -> zbus::Result<Vec<String>>;
    fn search_images(
        &self,
        term: &str,
        source: &str,
        limit: u32,
        options: Options<'_>,
    ) -> zbus::Result<Vec<Dict>>;
    fn list_repositories(
        &self,
        filter: &str,
        with_tags: bool,
        options: Options<'_>,
    ) -> zbus::Result<Vec<Dict>>;
    fn list_tags(&self, repository: &str, options: Options<'_>) -> zbus::Result<Vec<String>>;
    fn list_machines(&self, all: bool) -> zbus::Result<Vec<Dict>>;
    fn start_machine(&self, name: &str, options: Options<'_>) -> zbus::Result<String>;
    fn stop_machine(&self, name: &str, options: Options<'_>) -> zbus::Result<String>;
    fn exec(
        &self,
        machine: &str,
        argv: &[String],
        user: &str,
        options: Options<'_>,
    ) -> zbus::Result<(HashMap<String, zbus::zvariant::OwnedFd>, OwnedObjectPath)>;
    fn shell(
        &self,
        machine: &str,
        user: &str,
    ) -> zbus::Result<(zbus::zvariant::OwnedFd, OwnedObjectPath)>;
    fn logs(&self, machine: &str, options: Options<'_>) -> zbus::Result<zbus::zvariant::OwnedFd>;
    fn list_network(&self) -> zbus::Result<(Dict, Vec<Dict>)>;
    fn network_up(&self) -> zbus::Result<Dict>;
    fn login(
        &self,
        registry: &str,
        username: &str,
        password: &str,
        options: Options<'_>,
    ) -> zbus::Result<Dict>;
    fn logout(&self, registry: &str) -> zbus::Result<bool>;

    #[zbus(signal)]
    fn job_output(&self, job: OwnedObjectPath, kind: String, line: String) -> zbus::Result<()>;
    #[zbus(signal)]
    fn job_removed(&self, job: OwnedObjectPath, result: String) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.nspawn.Job",
    default_service = "org.nspawn",
    gen_blocking = false
)]
pub trait Job {
    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn error(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn result(&self) -> zbus::Result<Dict>;
}

#[zbus::proxy(
    interface = "org.nspawn.Process",
    default_service = "org.nspawn",
    gen_blocking = false
)]
pub trait Process {
    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn exit_status(&self) -> zbus::Result<i32>;
}

pub struct Client {
    pub connection: zbus::Connection,
    pub manager: ManagerProxy<'static>,
}

impl Client {
    pub async fn connect() -> Result<Self> {
        let connection = zbus::Connection::system()
            .await
            .context("connecting to the system bus")?;
        let manager = ManagerProxy::new(&connection)
            .await
            .context("reaching org.nspawn")?;
        Ok(Client {
            connection,
            manager,
        })
    }

    /// Runs a job to its end: its lines go to the terminal as they arrive, and its
    /// result comes back, or its error. `start` is called once the signals are being
    /// listened to, so that no line is missed.
    pub async fn run_job<F, Fut>(&self, start: F) -> Result<Dict>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = zbus::Result<OwnedObjectPath>>,
    {
        let mut output = self
            .manager
            .receive_job_output()
            .await
            .context("listening for job output")?;
        let mut removed = self
            .manager
            .receive_job_removed()
            .await
            .context("listening for job results")?;
        let job = start().await.map_err(error)?;
        let result = loop {
            tokio::select! {
                Some(signal) = output.next() => {
                    let Ok(args) = signal.args() else { continue };
                    if *args.job() != job { continue }
                    match args.kind().as_str() {
                        "note" => eprintln!("{}", args.line()),
                        _ => println!("{}", args.line()),
                    }
                }
                Some(signal) = removed.next() => {
                    let Ok(args) = signal.args() else { continue };
                    if *args.job() != job { continue }
                    break args.result().clone();
                }
                else => anyhow::bail!("the service went away while the job ran"),
            }
        };
        let proxy = JobProxy::builder(&self.connection)
            .path(job.clone())?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await?;
        if result == "done" {
            Ok(proxy.result().await.map_err(error)?)
        } else {
            Err(anyhow!("{}", proxy.error().await.map_err(error)?))
        }
    }
}

/// What the service's error reads like here: the message alone for the service's own
/// errors, and a word of advice when the service is not there or refuses the caller.
pub fn error(e: zbus::Error) -> anyhow::Error {
    match &e {
        zbus::Error::MethodError(name, message, _) => {
            match name.as_str() {
                "org.freedesktop.DBus.Error.ServiceUnknown" => {
                    anyhow!("the nspawn service is not on the system bus; run: sudo nspawn daemon --install")
                }
                "org.freedesktop.DBus.Error.AccessDenied" => {
                    anyhow!("the bus refused the call: nspawn needs root to talk to its service for now")
                }
                _ => anyhow!("{}", message.clone().unwrap_or_else(|| name.to_string())),
            }
        }
        _ => anyhow!("{e}"),
    }
}

/// The registry and CA certificate the command line was given, for the service to use
/// on this call instead of its own configuration.
pub fn registry_options(config: &Config) -> Options<'_> {
    let mut options = Options::new();
    options.insert("registry", Value::from(config.registry.as_str()));
    if let Some(ca) = &config.ca_cert {
        options.insert("ca_cert", Value::from(ca.to_string_lossy().into_owned()));
    }
    options
}

pub fn string(dict: &Dict, key: &str) -> String {
    dict.get(key)
        .and_then(|v| String::try_from(v.clone()).ok())
        .unwrap_or_default()
}

pub fn strings(dict: &Dict, key: &str) -> Vec<String> {
    dict.get(key)
        .and_then(|v| Vec::<String>::try_from(v.clone()).ok())
        .unwrap_or_default()
}

pub fn u64(dict: &Dict, key: &str) -> u64 {
    dict.get(key)
        .and_then(|v| u64::try_from(v.clone()).ok())
        .unwrap_or(0)
}

pub fn bool(dict: &Dict, key: &str) -> bool {
    dict.get(key)
        .and_then(|v| bool::try_from(v.clone()).ok())
        .unwrap_or(false)
}

/// "-" for what is not there, as the tables show it.
pub fn dash(text: String) -> String {
    if text.is_empty() {
        "-".to_string()
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::values::v;

    fn method_error(name: &str, message: Option<&str>) -> zbus::Error {
        let call = zbus::message::Message::method_call("/org/nspawn", "Test")
            .unwrap()
            .build(&())
            .unwrap();
        zbus::Error::MethodError(
            zbus::names::OwnedErrorName::try_from(name).unwrap(),
            message.map(|m| m.to_string()),
            call,
        )
    }

    #[test]
    fn service_errors_read_like_the_command_line() {
        let failed = error(method_error(
            "org.nspawn.Error.Failed",
            Some("no image named web; see nspawn images ls, or pull one"),
        ));
        assert_eq!(
            failed.to_string(),
            "no image named web; see nspawn images ls, or pull one"
        );
        let missing = error(method_error(
            "org.freedesktop.DBus.Error.ServiceUnknown",
            Some("The name org.nspawn was not provided by any .service files"),
        ));
        assert!(missing.to_string().contains("nspawn daemon --install"));
        let denied = error(method_error(
            "org.freedesktop.DBus.Error.AccessDenied",
            None,
        ));
        assert!(denied.to_string().contains("root"));
        let nameless = error(method_error("org.example.Odd", None));
        assert_eq!(nameless.to_string(), "org.example.Odd");
    }

    #[test]
    fn dictionaries_are_read_by_name_and_type() {
        let dict: Dict = HashMap::from([
            ("name".to_string(), v("web")),
            ("size".to_string(), v(42u64)),
            ("leader".to_string(), v(7u64)),
            ("running".to_string(), v(true)),
            ("ports".to_string(), v(vec!["80->80/tcp".to_string()])),
        ]);
        assert_eq!(string(&dict, "name"), "web");
        assert_eq!(string(&dict, "missing"), "");
        assert_eq!(u64(&dict, "size"), 42);
        assert_eq!(u64(&dict, "leader"), 7);
        assert_eq!(u64(&dict, "name"), 0, "a string is not a number");
        assert!(bool(&dict, "running"));
        assert!(!bool(&dict, "missing"));
        assert_eq!(strings(&dict, "ports"), ["80->80/tcp"]);
        assert_eq!(dash(String::new()), "-");
        assert_eq!(dash("x".to_string()), "x");
    }

    #[test]
    fn registry_options_carry_what_the_command_line_got() {
        let mut config = Config::load(None, Some("hub.example:8443".into()), None).unwrap();
        config.ca_cert = Some(std::path::PathBuf::from("/etc/ca.crt"));
        let options = registry_options(&config);
        assert_eq!(
            options.get("registry"),
            Some(&Value::from("hub.example:8443"))
        );
        assert_eq!(options.get("ca_cert"), Some(&Value::from("/etc/ca.crt")));
    }
}
