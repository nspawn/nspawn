//! The `--flag` words of an instruction (bflag.go).

use super::compat::{to_lower, with_suggestion};
use super::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagType {
    Bool,
    String,
    /// A flag that may be given several times.
    Strings,
}

/// Handle of a flag added to a [`BFlags`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlagId(usize);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flag {
    name: String,
    flag_type: FlagType,
    /// The value of a bool (`"true"` or `"false"`) or string flag.
    pub value: String,
    /// The values of a strings flag, in order.
    pub string_values: Vec<String>,
}

impl Flag {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn flag_type(&self) -> FlagType {
        self.flag_type
    }

    /// The value of a bool flag.
    ///
    /// # Panics
    ///
    /// When the flag is not a bool flag, as in the reference.
    pub fn is_true(&self) -> bool {
        assert!(
            self.flag_type == FlagType::Bool,
            "Trying to use IsTrue on a non-boolean: {}",
            self.name
        );
        self.value == "true"
    }
}

/// The flags an instruction accepts and the ones it was given.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BFlags {
    /// The flag words as written, `--name=value` or `--name`.
    pub args: Vec<String>,
    flags: Vec<Flag>,
    used: Vec<usize>,
    err: Option<String>,
}

impl BFlags {
    pub fn new() -> BFlags {
        BFlags::default()
    }

    pub fn with_args(args: Vec<String>) -> BFlags {
        BFlags {
            args,
            ..BFlags::default()
        }
    }

    pub fn add_bool(&mut self, name: &str, default: bool) -> FlagId {
        self.add_flag(name, FlagType::Bool, if default { "true" } else { "false" })
    }

    pub fn add_string(&mut self, name: &str, default: &str) -> FlagId {
        self.add_flag(name, FlagType::String, default)
    }

    pub fn add_strings(&mut self, name: &str) -> FlagId {
        self.add_flag(name, FlagType::Strings, "")
    }

    /// Adding a flag twice is reported by [`BFlags::parse`]; the second
    /// definition is ignored.
    fn add_flag(&mut self, name: &str, flag_type: FlagType, default: &str) -> FlagId {
        if let Some(i) = self.flags.iter().position(|f| f.name == name) {
            self.err = Some(format!("Duplicate flag defined: {name}"));
            return FlagId(i);
        }
        self.flags.push(Flag {
            name: name.to_string(),
            flag_type,
            value: default.to_string(),
            string_values: Vec::new(),
        });
        FlagId(self.flags.len() - 1)
    }

    pub fn flag(&self, id: FlagId) -> &Flag {
        &self.flags[id.0]
    }

    pub fn is_used(&self, id: FlagId) -> bool {
        self.used.contains(&id.0)
    }

    /// The names of the flags given, in the order they first appear.
    pub fn used(&self) -> Vec<String> {
        self.used
            .iter()
            .map(|&i| self.flags[i].name.clone())
            .collect()
    }

    /// Checks the flag words against the flags added and stores their
    /// values. A `--` word ends the flags.
    pub fn parse(&mut self) -> Result<(), Error> {
        if let Some(err) = &self.err {
            return Err(Error::new(format!("error setting up flags: {err}")));
        }
        for a in &self.args {
            if a == "--" {
                return Ok(());
            }
            if !a.starts_with("--") {
                return Err(Error::new(format!("arg should start with -- : {a}")));
            }
            let (flag_name, value, has_value) = match a.split_once('=') {
                Some((n, v)) => (n, v, true),
                None => (a.as_str(), "", false),
            };
            let arg = &flag_name[2..];
            let Some(idx) = self.flags.iter().position(|f| f.name == arg) else {
                let names: Vec<&str> = self.flags.iter().map(|f| f.name.as_str()).collect();
                return Err(Error::new(with_suggestion(
                    format!("unknown flag: {flag_name}"),
                    arg,
                    &names,
                    true,
                )));
            };
            let flag = &mut self.flags[idx];
            if self.used.contains(&idx) {
                if flag.flag_type != FlagType::Strings {
                    return Err(Error::new(format!("duplicate flag specified: {flag_name}")));
                }
            } else {
                self.used.push(idx);
            }
            match flag.flag_type {
                FlagType::Bool => {
                    if has_value && value.is_empty() {
                        return Err(Error::new(format!("missing a value on flag: {flag_name}")));
                    }
                    match to_lower(value).as_str() {
                        "true" | "" => flag.value = "true".to_string(),
                        "false" => flag.value = "false".to_string(),
                        _ => {
                            return Err(Error::new(format!(
                                "expecting boolean value for flag {flag_name}, not: {value}"
                            )))
                        }
                    }
                }
                FlagType::String => {
                    if !has_value {
                        return Err(Error::new(format!("missing a value on flag: {flag_name}")));
                    }
                    flag.value = value.to_string();
                }
                FlagType::Strings => {
                    if !has_value {
                        return Err(Error::new(format!("missing a value on flag: {flag_name}")));
                    }
                    flag.string_values.push(value.to_string());
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn builder_flags() {
        let mut bf = BFlags::new();
        bf.args = vec![];
        bf.parse().expect("Test1 was supposed to work");

        let mut bf = BFlags::new();
        bf.args = args(&["--"]);
        bf.parse().expect("Test2 was supposed to work");

        let mut bf = BFlags::new();
        let fl_str1 = bf.add_string("str1", "");
        let fl_bool1 = bf.add_bool("bool1", false);
        bf.parse().expect("Test3 was supposed to work");
        assert!(!bf.is_used(fl_str1));
        assert!(!bf.is_used(fl_bool1));

        let mut bf = BFlags::new();
        let fl_str1 = bf.add_string("str1", "HI");
        let fl_bool1 = bf.add_bool("bool1", false);
        bf.parse().expect("Test4 was supposed to work");
        assert_eq!(bf.flag(fl_str1).value, "HI");
        assert!(!bf.flag(fl_bool1).is_true());
        assert!(!bf.is_used(fl_str1));
        assert!(!bf.is_used(fl_bool1));

        let mut bf = BFlags::new();
        bf.add_string("str1", "HI");
        bf.args = args(&["--str1"]);
        assert!(bf.parse().is_err());

        let mut bf = BFlags::new();
        let fl_str1 = bf.add_string("str1", "HI");
        bf.args = args(&["--str1="]);
        bf.parse().unwrap();
        assert_eq!(bf.flag(fl_str1).value, "");

        let mut bf = BFlags::new();
        let fl_str1 = bf.add_string("str1", "HI");
        bf.args = args(&["--str1=BYE"]);
        bf.parse().unwrap();
        assert_eq!(bf.flag(fl_str1).value, "BYE");

        let mut bf = BFlags::new();
        let fl_bool1 = bf.add_bool("bool1", false);
        bf.args = args(&["--bool1"]);
        bf.parse().unwrap();
        assert!(bf.flag(fl_bool1).is_true());

        let mut bf = BFlags::new();
        let fl_bool1 = bf.add_bool("bool1", false);
        bf.args = args(&["--bool1=true"]);
        bf.parse().unwrap();
        assert!(bf.flag(fl_bool1).is_true());

        let mut bf = BFlags::new();
        let fl_bool1 = bf.add_bool("bool1", false);
        bf.args = args(&["--bool1=false"]);
        bf.parse().unwrap();
        assert!(!bf.flag(fl_bool1).is_true());

        let mut bf = BFlags::new();
        bf.add_bool("bool1", false);
        bf.args = args(&["--bool1=false1"]);
        assert!(bf.parse().is_err());

        let mut bf = BFlags::new();
        bf.add_bool("bool1", false);
        bf.args = args(&["--bool2"]);
        assert!(bf.parse().is_err());

        let mut bf = BFlags::new();
        let fl_str1 = bf.add_string("str1", "HI");
        let fl_bool1 = bf.add_bool("bool1", false);
        bf.args = args(&["--bool1", "--str1=BYE"]);
        bf.parse().unwrap();
        assert_eq!(bf.flag(fl_str1).value, "BYE");
        assert!(bf.flag(fl_bool1).is_true());

        let mut bf = BFlags::new();
        bf.add_bool("bool1", false);
        bf.add_bool("bool2", false);
        bf.add_bool("bool3", false);
        bf.add_bool("bool4", true);
        bf.add_bool("bool5", true);
        bf.add_string("str1", "");
        bf.add_string("str2", "");
        bf.add_string("str3", "def3");
        bf.add_string("str4", "def4");
        bf.args = args(&[
            "--bool2=false",
            "--bool3",
            "--bool4=true",
            "--bool5",
            "--str2= ",
            "--str3=def3",
            "--str4=my-val",
        ]);
        bf.parse().unwrap();
        let mut used = bf.used();
        used.sort();
        assert_eq!(
            used.join(", "),
            "bool2, bool3, bool4, bool5, str2, str3, str4"
        );
    }

    #[test]
    fn error_messages() {
        let parse = |defs: &dyn Fn(&mut BFlags), a: &[&str]| {
            let mut bf = BFlags::with_args(args(a));
            defs(&mut bf);
            bf.parse().unwrap_err().to_string()
        };
        let mount = |bf: &mut BFlags| {
            bf.add_strings("mount");
            bf.add_bool("link", false);
            bf.add_string("from", "");
        };
        assert_eq!(
            parse(&mount, &["--mont=x"]),
            "unknown flag: --mont (did you mean mount?)"
        );
        assert_eq!(parse(&mount, &["--zzz"]), "unknown flag: --zzz");
        assert_eq!(parse(&mount, &["x"]), "arg should start with -- : x");
        assert_eq!(
            parse(&mount, &["--from=a", "--from=b"]),
            "duplicate flag specified: --from"
        );
        assert_eq!(
            parse(&mount, &["--from"]),
            "missing a value on flag: --from"
        );
        assert_eq!(
            parse(&mount, &["--link="]),
            "missing a value on flag: --link"
        );
        assert_eq!(
            parse(&mount, &["--link=yes"]),
            "expecting boolean value for flag --link, not: yes"
        );
        let twice = |bf: &mut BFlags| {
            bf.add_string("a", "");
            bf.add_bool("a", false);
        };
        assert_eq!(
            parse(&twice, &[]),
            "error setting up flags: Duplicate flag defined: a"
        );

        let mut bf = BFlags::with_args(args(&[
            "--mount=a",
            "--link=TRUE",
            "--mount=b",
            "--",
            "--x",
        ]));
        let m = bf.add_strings("mount");
        let l = bf.add_bool("link", false);
        bf.parse().unwrap();
        assert_eq!(bf.flag(m).string_values, vec!["a".to_string(), "b".into()]);
        assert!(bf.flag(l).is_true());
        assert_eq!(bf.used(), vec!["mount".to_string(), "link".into()]);
    }
}
