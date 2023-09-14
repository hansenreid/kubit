use anyhow::{bail, Result};
use clap::Subcommand;
use kube::ResourceExt;
use std::fs::{self, File};
use std::io::{stdout, IsTerminal, Read, Write};
use std::os::unix::prelude::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use tempfile::NamedTempFile;
use std::future::Future;

use crate::{
    apply::{self, KUBIT_APPLIER_FIELD_MANAGER},
    render,
    resources::AppInstance,
    scripting::Script,
};

#[derive(Clone, Subcommand)]
pub enum Local {
    /// Applies the template locally
    Apply {
        /// Path to the file containing a (YAML) AppInstance manifest.
        app_instance: String,

        /// Dry run
        #[clap(long)]
        dry_run: Option<DryRun>,

        /// Show diff before applying. If in tty, interactively ask if you want to continue.
        #[clap(long("diff"), default_value = "false")]
        pre_diff: bool,

        /// Override the package image field in the spec
        #[clap(long)]
        package_image: Option<String>,
    },
}

#[derive(Clone, clap::ValueEnum)]
pub enum DryRun {
    Render,
    Diff,
    Script,
}

pub async fn run(local: &Local, impersonate_user: &Option<String>) -> Result<()> {
    match local {
        Local::Apply {
            app_instance,
            dry_run,
            package_image,
            pre_diff,
        } => {
            apply(
                app_instance,
                dry_run,
                package_image,
                impersonate_user,
                *pre_diff,
                true,
            ).await?;
        }
    };
    Ok(())
}

/// This trait allows us to close the temporary file but not delete it yet
trait DeferredDelete {
    fn close(self: Box<Self>) -> std::io::Result<Option<DeferredDeleteHandle>>;
}

struct DeferredDeleteHandle {
    path: PathBuf,
}

impl Drop for DeferredDeleteHandle {
    fn drop(&mut self) {
        fs::remove_file(self.path.clone()).unwrap()
    }
}

impl DeferredDelete for NamedTempFile {
    fn close(self: Box<Self>) -> std::io::Result<Option<DeferredDeleteHandle>> {
        let path = self.path().to_path_buf();
        self.persist(path.clone())?;
        Ok(Some(DeferredDeleteHandle { path }))
    }
}

struct NopDeferredDelete<W>(W);

impl<W> DeferredDelete for NopDeferredDelete<W> {
    fn close(self: Box<Self>) -> std::io::Result<Option<DeferredDeleteHandle>> {
        Ok(None)
    }
}

impl<W> Write for NopDeferredDelete<W>
where
    W: Write,
{
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

trait WriteClose: Write + DeferredDelete {}
impl<W: Write> WriteClose for NopDeferredDelete<W> {}
impl WriteClose for NamedTempFile {}

/// Generate a script that runs kubecfg show and kubectl apply and runs it.
pub async fn apply(
    app_instance: &str,
    dry_run: &Option<DryRun>,
    package_image: &Option<String>,
    impersonate_user: &Option<String>,
    pre_diff: bool,
    is_local: bool,
) -> Box<dyn Future<Output = ()>, Result<()>> {
    let (mut output, path): (Box<dyn WriteClose>, _) = if matches!(dry_run, Some(DryRun::Script)) {
        (Box::new(NopDeferredDelete(stdout())), None)
    } else {
        let tmp = tempfile::Builder::new().suffix(".sh").tempfile()?;
        let path = tmp.path().to_path_buf();
        (Box::new(tmp), Some(path))
    };

    let overlay_file_name = app_instance;
    let file = File::open(overlay_file_name)?;
    let mut app_instance: AppInstance = serde_yaml::from_reader(file)?;

    if let Some(package_image) = package_image {
        app_instance.spec.package.image = package_image.clone();
    }

    if pre_diff {
        if dry_run.is_some() {
            bail!("--diff and --dry-run are mutually exclusive");
        }
        apply(
            overlay_file_name,
            &Some(DryRun::Diff),
            package_image,
            impersonate_user,
            false,
            true,
        ).await?;
        if !confirm_continue() {
            return Ok(());
        }
    }

    let mut steps: Vec<Script> = vec![];

    if is_local {
        steps.extend([
            render::script(&app_instance, overlay_file_name, None, is_local).await?
                | match dry_run {
                    Some(DryRun::Render) => Script::from_str("cat"),
                    Some(DryRun::Diff) => diff(&app_instance)?,
                    Some(DryRun::Script) | None => {
                        apply::script(&app_instance, "-", impersonate_user, is_local)?
                    }
                },
        ]);
    } else {
        steps.extend([
            Script::from_str("export KUBECTL_APPLYSET=true"),
            render::script(&app_instance, overlay_file_name, None, is_local).await?
                | match dry_run {
                    Some(DryRun::Render) => Script::from_str("cat"),
                    Some(DryRun::Diff) => diff(&app_instance)?,
                    Some(DryRun::Script) | None => {
                        apply::script(&app_instance, "-", impersonate_user, is_local)?
                    }
                },
        ]);
    }

    let script: Script = steps.into_iter().sum();

    writeln!(output, "{script}")?;

    // close the file but don't delete it until _deferred_delete_handle local var is in scope.
    let _deferred_delete_handle = output.close()?;

    if let Some(path) = path {
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
        Command::new(path).status()?;
    }

    Ok(())
}

fn diff(app_instance: &AppInstance) -> Result<Script> {
    let applyset_id = get_applyset_id(app_instance)?;
    let remove_labels = Script::from_str(&format!(
        "apply_label applyset.kubernetes.io/part-of={applyset_id}"
    ));
    let diff = format!("kubectl diff -f - --server-side --force-conflicts --field-manager={KUBIT_APPLIER_FIELD_MANAGER}");
    let diff = Script::from_str(&diff);
    let script = (apply_label_workaround() + (remove_labels | diff)).subshell();
    Ok(script)
}

// Workaround for issue: https://github.com/kubernetes/kubectl/issues/1265
fn apply_label_workaround() -> Script {
    Script::from_str(
        r#"apply_label() {
        kubectl label --local -f - -o json "$1" \
        | jq -c . \
        | while read -r line; do echo '---'; echo "$line" | yq eval -P; done
      }"#,
    )
}

fn get_applyset_id(app_instance: &AppInstance) -> Result<String> {
    // kubectl -n influxdb get secret influxdb -o jsonpath="{.metadata.labels.applyset\.kubernetes\.io/id}"
    let out = Command::new("kubectl")
        .arg("get")
        .arg("secret")
        .arg("-n")
        .arg(app_instance.namespace_any())
        .arg(app_instance.name_any())
        .arg("-o")
        .arg("jsonpath={.metadata.labels.applyset\\.kubernetes\\.io/id}")
        .output()?
        .stdout;
    Ok(String::from_utf8(out)?)
}

pub fn confirm_continue() -> bool {
    if !std::io::stdout().is_terminal() {
        return true;
    }

    print!("Apply? [y/N] ");
    std::io::stdout().flush().unwrap();

    /*
    let is_tty = unsafe { libc::isatty(libc::STDIN_FILENO as i32) } != 0;
    if !is_tty {
        return true;
    }
    */

    let mut buffer = [0; 1];
    std::io::stdin().read_exact(&mut buffer).unwrap();
    matches!(buffer[0], b'y' | b'Y')
}
