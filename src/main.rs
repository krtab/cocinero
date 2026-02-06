use std::cell::LazyCell;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::{File, create_dir_all, remove_dir_all};
use std::io::{BufWriter, Error as IoError};
use std::io::{ErrorKind as IoErrorKind, Write as _};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
use clap::Parser;
use handlebars::Handlebars;
use handlebars::RenderError;
use handlebars::TemplateError;
use itertools::Itertools;
use log::trace;
use serde::Deserialize;
use thiserror::Error;
use toml::Value;
use toml::map::Map;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipe {
    #[serde(default)]
    packages: Vec<String>,
    #[serde(default)]
    systemd: Vec<String>,
    #[serde(default)]
    template_vars: Vec<toml::Table>,
    #[serde(default)]
    steps: Vec<Step>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
#[serde(tag = "kind")]
#[serde(deny_unknown_fields)]
enum Step {
    #[serde(alias = "copy")]
    Install {
        #[serde(default)]
        template: bool,
        #[serde(flatten)]
        install: Install,
    },
    Shell {
        #[serde(default)]
        template: bool,
        cmd: String,
    },
    Run {
        #[serde(default)]
        template: bool,
        script: Utf8PathBuf,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Install {
    src: Utf8PathBuf,
    dest: Utf8PathBuf,
    mode: Option<String>,
}

#[derive(Debug, Error)]
enum ReceipeLoadError {
    #[error("No receipe")]
    NoReceipe,
    #[error("Io error {error} happened while {context}")]
    Io { error: IoError, context: String },
    #[error("Couldn't parse receipe")]
    ReceipeParseError(#[from] toml::de::Error),
}

impl ReceipeLoadError {
    fn io_context(context: impl Into<String>) -> impl FnOnce(IoError) -> ReceipeLoadError {
        let context = context.into();
        |error| ReceipeLoadError::Io { error, context }
    }
}

impl Receipe {
    fn try_load(receipe_toml_path: impl AsRef<Path>) -> Result<Self, ReceipeLoadError> {
        if !receipe_toml_path
            .as_ref()
            .try_exists()
            .map_err(ReceipeLoadError::io_context("Checking if path exists"))?
        {
            return Err(ReceipeLoadError::NoReceipe);
        }
        let receipe = {
            let s = std::fs::read_to_string(receipe_toml_path)
                .map_err(ReceipeLoadError::io_context("Loading receipe"))?;
            toml::from_str(&s)?
        };
        Ok(receipe)
    }
}

#[derive(Debug, clap::Parser)]
struct CliArgs {
    receipes: Utf8PathBuf,
    #[clap(long)]
    target: Option<Utf8PathBuf>,
}

fn new_buf_file(p: impl AsRef<Path>) -> std::io::Result<BufWriter<File>> {
    File::create(p).map(BufWriter::new)
}

fn copy_create_dir(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> anyhow::Result<()> {
    let src = src.as_ref();
    let dest = dst.as_ref();
    std::fs::create_dir_all(dest.parent().unwrap_or(Path::new("/")))?;
    std::fs::copy(src, dest)?;
    Ok(())
}

struct TemplateEnv {
    inner: Handlebars<'static>,
    counter: u64,
}

struct TemplateId(String);

impl TemplateEnv {
    fn new() -> Self {
        let mut inner = Handlebars::new();
        inner.set_strict_mode(true);
        Self { inner, counter: 0 }
    }

    fn next_template_name(&mut self) -> String {
        self.counter += 1;
        format!("template_{}", self.counter)
    }

    fn register_template_string(
        &mut self,
        template: impl AsRef<str>,
    ) -> Result<TemplateId, TemplateError> {
        let name = self.next_template_name();
        self.inner
            .register_template_string(&name, template.as_ref())?;
        Ok(TemplateId(name))
    }

    fn register_template_file(
        &mut self,
        template: impl AsRef<Path>,
    ) -> Result<TemplateId, TemplateError> {
        let name = self.next_template_name();
        self.inner
            .register_template_file(&name, template.as_ref())?;
        Ok(TemplateId(name))
    }

    fn render(
        &self,
        template_id: &TemplateId,
        data: &Map<String, Value>,
    ) -> Result<String, RenderError> {
        self.inner.render(&template_id.0, data)
    }

    fn render_to_write(
        &self,
        template_id: &TemplateId,
        data: &Map<String, Value>,
        w: impl std::io::Write,
    ) -> Result<(), RenderError> {
        self.inner.render_to_write(&template_id.0, data, w)
    }
}

fn main() -> anyhow::Result<()> {
    env_logger::init();
    let cli_args = CliArgs::parse();
    let all_receipes = load_all_receipes(&cli_args)?;

    let target = cli_args
        .target
        .unwrap_or_else(|| Utf8PathBuf::from("cocinero_target"));

    match remove_dir_all(&target) {
        Ok(()) => (),
        Err(e) if e.kind() == IoErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    create_dir_all(&target)?;
    let mut script = create_script(&target.join("cook.sh"))?;
    writeln!(script, "echo 'starting to cook'")?;

    let mut template_env = LazyCell::new(TemplateEnv::new);

    let packages = all_receipes.values().flat_map(|u| &u.packages);
    for package_chunk in &packages.chunks(64) {
        write!(script, "apt-get install")?;
        for pkg in package_chunk {
            write!(script, " {pkg}")?;
        }
        writeln!(script,)?;
    }
    writeln!(script)?;
    for (receipe_dir_name, receipe) in &all_receipes {
        if receipe.steps.is_empty() {
            continue;
        }
        let orig_receipe_dir_path = cli_args.receipes.join(receipe_dir_name);
        let target_receipe_dir_path = target.join(receipe_dir_name);
        std::fs::create_dir_all(&target_receipe_dir_path)?;
        let receipe_script_path = target_receipe_dir_path.join("_cook.sh");
        let mut receipe_script = create_script(&receipe_script_path)?;
        writeln!(script, r#"echo 'running receipe "{receipe_dir_name}"'"#)?;
        writeln!(script, "(cd {receipe_dir_name} && ./_cook.sh)")?;
        for step in &receipe.steps {
            match step {
                Step::Install {
                    template: false,
                    install,
                } => perform_install_file(
                    &mut receipe_script,
                    &orig_receipe_dir_path,
                    &target_receipe_dir_path,
                    install,
                )?,
                Step::Install {
                    template: true,
                    install,
                } => {
                    perform_template_install(
                        &mut receipe_script,
                        &orig_receipe_dir_path,
                        &target_receipe_dir_path,
                        install,
                        &mut template_env,
                        &receipe.template_vars,
                    )?;
                }
                Step::Shell {
                    template: false,
                    cmd,
                } => {
                    writeln!(&mut receipe_script, "{cmd}")?;
                }
                Step::Shell {
                    template: true,
                    cmd,
                } => {
                    let cmd_tmplt = template_env.register_template_string(cmd)?;
                    for vars in &receipe.template_vars {
                        template_env.render_to_write(&cmd_tmplt, vars, &mut receipe_script)?;
                        writeln!(&mut receipe_script)?;
                    }
                }
                Step::Run {
                    template: false,
                    script,
                } => {
                    let src_path = orig_receipe_dir_path.join(script);
                    let target_path = target_receipe_dir_path.join(script);
                    copy_create_dir(src_path, target_path)?;
                    writeln!(&mut receipe_script, "./{script}")?;
                }
                Step::Run {
                    template: true,
                    script,
                } => {
                    let src_path = orig_receipe_dir_path.join(script);
                    let file_template = template_env.register_template_file(src_path)?;
                    for (i, vars) in receipe.template_vars.iter().enumerate() {
                        let dest_name = script.with_added_extension(format!("{i}"));
                        let target_path = target_receipe_dir_path.join(&dest_name);
                        let mut f = new_buf_file(target_path)?;
                        template_env.render_to_write(&file_template, vars, &mut f)?;
                        chmod_plus_x(f.get_mut())?;
                        writeln!(&mut receipe_script, "./{dest_name}")?;
                    }
                }
            }
        }
    }
    writeln!(script,)?;
    for receipe in all_receipes.values() {
        for unit in &receipe.systemd {
            writeln!(script, "systemctl enable --now {unit}")?;
            writeln!(script, "systemctl reload-or-restart {unit}")?;
        }
    }
    Ok(())
}

fn create_script(p: &Utf8Path) -> anyhow::Result<BufWriter<File>> {
    create_dir_all(p.parent().unwrap_or(Utf8Path::new("/")))?;
    let mut buf_file = new_buf_file(p)?;
    for l in [
        "#!/usr/bin/bash",
        "",
        "# Generated by cocinero",
        "",
        "set -e",
        "",
    ] {
        writeln!(buf_file, "{l}")?
    }
    let file = buf_file.get_mut();
    chmod_plus_x(file)?;
    Ok(buf_file)
}

fn chmod_plus_x(file: &mut File) -> Result<(), anyhow::Error> {
    let new_mode = file.metadata()?.permissions().mode() | 0o500;
    file.set_permissions(PermissionsExt::from_mode(new_mode))?;
    Ok(())
}

fn load_all_receipes(cli_args: &CliArgs) -> Result<HashMap<Utf8PathBuf, Receipe>, anyhow::Error> {
    let mut all_receipes = HashMap::new();
    for entry in cli_args.receipes.read_dir()? {
        let entry = entry?;
        let path: Utf8PathBuf = entry.path().try_into()?;
        let metadata = std::fs::metadata(&path)?;
        let name: Utf8PathBuf = entry.file_name().try_into()?;
        if !metadata.is_dir() {
            trace!("Ignoring non directory entry : {}", name);
            continue;
        }
        let receipe_toml_path = path.join("receipe.toml");
        let receipe = match Receipe::try_load(&receipe_toml_path) {
            Ok(r) => r,
            Err(ReceipeLoadError::NoReceipe) => {
                trace!("Ignoring directory {} without receipe.toml", name);
                continue;
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("While parsing receipe {}", receipe_toml_path));
            }
        };
        let duplicate = all_receipes.insert(name, receipe).is_some();
        debug_assert!(!duplicate, "Duplicate entry: {}", path)
    }
    Ok(all_receipes)
}

fn perform_template_install(
    receipe_script: &mut BufWriter<File>,
    orig_receipe_dir_path: &Utf8PathBuf,
    target_receipe_dir_path: &Utf8PathBuf,
    install: &Install,
    template_env: &mut TemplateEnv,
    template_vars: &[Map<String, Value>],
) -> Result<(), anyhow::Error> {
    let Install { src, dest, mode } = install;
    let orig_src_path = orig_receipe_dir_path.join(src);
    let dest_template = template_env.register_template_string(dest)?;
    let file_template = template_env.register_template_file(orig_src_path)?;
    for var in template_vars {
        let dest = template_env.render(&dest_template, var)?;
        let dest_mangled = dest.replace('/', "__");
        let target_path = target_receipe_dir_path.join(&dest_mangled);
        let target_file = new_buf_file(&target_path)?;
        template_env.render_to_write(&file_template, var, target_file)?;
        check_managed_disclaimer(&target_path)?;
        install_in_script(
            receipe_script,
            &Install {
                src: dest_mangled.into(),
                dest: dest.into(),
                mode: mode.clone(),
            },
        )?;
    }
    Ok(())
}

fn perform_install_file(
    script: &mut BufWriter<File>,
    orig_receipe_dir_path: &Utf8Path,
    target_receipe_dir_path: &Utf8Path,
    install: &Install,
) -> anyhow::Result<()> {
    let Install { src, .. } = install;
    let src_path = orig_receipe_dir_path.join(src);
    let target_path = target_receipe_dir_path.join(src);
    copy_create_dir(src_path, &target_path)?;
    check_managed_disclaimer(&target_path)?;
    install_in_script(script, install)?;
    Ok(())
}

fn check_managed_disclaimer(p: &Utf8Path) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(p)?;
    if !content.contains("managed by cocinero") {
        println!(r#"File {} has no "managed by cocinero" disclaimer."#, p);
    }
    Ok(())
}

fn install_in_script(script: &mut BufWriter<File>, install: &Install) -> Result<(), anyhow::Error> {
    let Install { dest, mode, src } = install;
    let mut args = String::new();
    if let Some(mode) = mode {
        write!(&mut args, " --mode={mode}").unwrap();
    }
    writeln!(script, "install  -D {src} {dest}")?;
    Ok(())
}
