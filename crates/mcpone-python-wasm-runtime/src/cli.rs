//! Command-style Python input. The execute(code) library export does not use it.

use std::io::Read;

#[derive(Debug, Clone)]
pub(crate) struct CliRunInput {
    pub code: String,
    pub filename: Option<String>,
    pub argv: Vec<String>,
    pub path0: String,
}

impl CliRunInput {
    pub fn from_args(argv: &[String], mut stdin: impl Read) -> Result<Self, String> {
        match argv.get(1).map(String::as_str) {
            Some("-c") => {
                let code = argv
                    .get(2)
                    .ok_or("python requires a code string after -c")?;
                Ok(Self {
                    code: code.clone(),
                    filename: None,
                    path0: String::new(),
                    argv: std::iter::once("-c".to_string())
                        .chain(argv[3..].iter().cloned())
                        .collect(),
                })
            }
            Some("-m") => {
                let module = argv
                    .get(2)
                    .ok_or("python requires a module name after -m")?;
                // runpy sets argv[0] to the located module's filename while it
                // runs, just as CPython does. Keep every following argument.
                Ok(Self {
                    code: format!(
                        "import runpy as _mcpone_runpy\n_mcpone_runpy.run_module({}, run_name='__main__', alter_sys=True)",
                        python_string_literal(module)
                    ),
                    filename: Some(format!("<module {module}>")),
                    path0: std::env::current_dir()
                        .map_err(|err| err.to_string())?
                        .to_string_lossy()
                        .into_owned(),
                    argv: std::iter::once("-m".to_string())
                        .chain(argv[3..].iter().cloned())
                        .collect(),
                })
            }
            Some(file) if !file.starts_with('-') => Ok(Self {
                code: std::fs::read_to_string(file)
                    .map_err(|err| format!("Failed to read python script '{file}': {err}"))?,
                filename: Some(file.to_string()),
                argv: argv[1..].to_vec(),
                path0: std::env::current_dir()
                    .map_err(|err| err.to_string())?
                    .join(file)
                    .parent()
                    .ok_or("script has no parent directory")?
                    .to_string_lossy()
                    .into_owned(),
            }),
            None | Some("-") if !argv.is_empty() => {
                let mut code = String::new();
                stdin
                    .read_to_string(&mut code)
                    .map_err(|err| format!("Failed to read Python code from stdin: {err}"))?;
                if code.trim().is_empty() {
                    return Err(
                        "no input: pass -c, a script path, or pipe code to stdin".to_string()
                    );
                }
                Ok(Self {
                    code,
                    filename: Some("<stdin>".to_string()),
                    path0: String::new(),
                    argv: if argv.len() == 1 {
                        vec![String::new()]
                    } else {
                        argv[1..].to_vec()
                    },
                })
            }
            _ => Err(
                "python supports -c <code>, -m <module>, a script path, or piped stdin".to_string(),
            ),
        }
    }
}

// Only the module name is part of generated bootstrap code. User arguments are
// installed as Unicode objects through the CPython API, never parsed as code.
fn python_string_literal(value: &str) -> String {
    let mut out = String::from("'");
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            ch if ch.is_control() => out.push_str(&format!("\\u{:04x}", u32::from(ch))),
            ch => out.push(ch),
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn command_arguments_are_data_and_do_not_prepend_user_source() {
        let code = "from __future__ import annotations\nprint('ok')";
        let input = CliRunInput::from_args(
            &args(&["python", "-c", code, "", "a b", "'\\\n\r\t\0", "雪"]),
            &b""[..],
        )
        .unwrap();
        assert_eq!(input.code, code);
        assert_eq!(input.argv, args(&["-c", "", "a b", "'\\\n\r\t\0", "雪"]));
        assert_eq!(input.filename, None);
        assert_eq!(input.path0, "");
    }

    #[test]
    fn file_keeps_supplied_name_and_trailing_arguments() {
        let input = CliRunInput::from_args(
            &args(&["python", file!(), "ens", "--domain", "example.eth"]),
            &b""[..],
        )
        .unwrap();
        assert_eq!(input.filename.as_deref(), Some(file!()));
        assert_eq!(input.code, std::fs::read_to_string(file!()).unwrap());
        assert!(std::path::Path::new(&input.path0).is_absolute());
        assert_eq!(
            input.argv,
            args(&[file!(), "ens", "--domain", "example.eth"])
        );
    }

    #[test]
    fn stdin_distinguishes_bare_and_explicit_marker() {
        let bare = CliRunInput::from_args(&args(&["python"]), &b"print('bare')"[..]).unwrap();
        assert_eq!(bare.argv, args(&[""]));
        let marked =
            CliRunInput::from_args(&args(&["python", "-", "", "a b"]), &b"print('stdin')"[..])
                .unwrap();
        assert_eq!(marked.argv, args(&["-", "", "a b"]));
        assert_eq!(marked.filename.as_deref(), Some("<stdin>"));
        assert_eq!(marked.code, "print('stdin')");
        assert_eq!(bare.path0, "");
        assert_eq!(marked.path0, "");
    }

    #[test]
    fn module_keeps_arguments_and_uses_runpy_filename() {
        let input = CliRunInput::from_args(
            &args(&["python", "-m", "json.tool", "--sort-keys", "a b.json"]),
            &b""[..],
        )
        .unwrap();
        assert_eq!(input.argv, args(&["-m", "--sort-keys", "a b.json"]));
        assert_eq!(
            input.path0,
            std::env::current_dir().unwrap().to_string_lossy()
        );
        assert!(
            input
                .code
                .contains("run_module('json.tool', run_name='__main__', alter_sys=True)")
        );
        assert_eq!(
            python_string_literal("'\\\n\r\t\0"),
            "'\\'\\\\\\u000a\\u000d\\u0009\\u0000'"
        );
    }

    #[test]
    fn missing_code_module_and_unsupported_options_fail() {
        for input in [
            &["python", "-c"][..],
            &["python", "-m"],
            &["python", "-unknown"],
            &[],
        ] {
            assert!(CliRunInput::from_args(&args(input), &b""[..]).is_err());
        }
    }
}
