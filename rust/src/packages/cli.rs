use super::files::join;
use super::{Context, Options, load_current, resolve};
use crate::flags::FlagSet;
use anyhow::{Result, bail};
use std::io::Write;

/// A context cancelled by the first SIGINT/SIGTERM, so the resolver stops at a safe point
/// instead of the process dying mid-publication.
pub(crate) fn signal_context() -> Result<Context> {
    use tokio::signal::unix::{SignalKind, signal};
    let ctx = Context::background();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let (mut interrupt, mut terminate) = {
        let _guard = runtime.enter();
        (
            signal(SignalKind::interrupt())?,
            signal(SignalKind::terminate())?,
        )
    };
    let cancel = ctx.clone();
    std::thread::spawn(move || {
        runtime.block_on(async {
            tokio::select! {
                _ = interrupt.recv() => {}
                _ = terminate.recv() => {}
            }
        });
        cancel.cancel();
        // Keep the handlers installed so later signals stay ignored.
        runtime.block_on(std::future::pending::<()>());
    });
    Ok(ctx)
}
fn default_unity_editor() -> String {
    if let Ok(configured) = std::env::var("UNITY_EDITOR")
        && !configured.is_empty()
    {
        return configured;
    }
    let home = std::env::var("HOME").unwrap_or_default();
    join(&[
        &home,
        "Unity/Hub/Editor",
        crate::ENGINE_VERSION,
        "Editor/Unity",
    ])
}
/// `unity-micro-editor resolve-packages`: prints the snapshot as one line of JSON. The
/// pinned Editor is the default source of built-in packages.
pub fn cli(args: impl IntoIterator<Item = String>) -> Result<()> {
    let mut flags = FlagSet::new("resolve-packages");
    flags.string("project", "", "explicit target Unity project");
    flags.string(
        "editor",
        &default_unity_editor(),
        "pinned SDK used only to read built-in package sources",
    );
    flags.bool("offline", false, "require locally verified package sources");
    flags.bool(
        "current",
        false,
        "validate the durable current snapshot without resolution/network",
    );
    flags.bool(
        "refresh-git",
        false,
        "explicitly resolve unlocked Git refs again",
    );
    flags.parse(args)?;
    let project = flags.get_string("project");
    if !flags.args().is_empty() || project.is_empty() {
        bail!("resolve-packages requires --project");
    }
    let ctx = signal_context()?;
    let editor = flags.get_string("editor");
    let snapshot = if flags.get_bool("current") {
        load_current(&ctx, &project, &editor, Options::default())?
    } else {
        let options = Options {
            offline: flags.get_bool("offline"),
            refresh_git: flags.get_bool("refresh-git"),
            ..Options::default()
        };
        resolve(&ctx, &project, &editor, options)?
    };
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, &snapshot)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}
