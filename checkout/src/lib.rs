use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::anyhow;
use anyhow::Context as _;

use librad::git::Urn;

use radicle_common::args::{Args, Error, Help};
use radicle_common::{fmt, keys, profile, project};
use radicle_terminal as term;

pub const HELP: Help = Help {
    name: "checkout",
    description: env!("CARGO_PKG_DESCRIPTION"),
    version: env!("CARGO_PKG_VERSION"),
    usage: r#"
Usage

    rad checkout <urn> [<option>...]

Options

    --help    Print help
"#,
};

pub struct Options {
    pub urn: Urn,
}

impl Args for Options {
    fn from_args(args: Vec<OsString>) -> anyhow::Result<(Self, Vec<OsString>)> {
        use lexopt::prelude::*;
        use std::str::FromStr;

        let mut parser = lexopt::Parser::from_args(args);
        let mut urn = None;

        while let Some(arg) = parser.next()? {
            match arg {
                Long("help") => return Err(Error::Help.into()),
                Value(val) if urn.is_none() => {
                    let val = val.to_string_lossy();
                    let val = Urn::from_str(&val).context(format!("invalid URN '{}'", val))?;

                    urn = Some(val);
                }
                _ => return Err(anyhow::anyhow!(arg.unexpected())),
            }
        }

        Ok((
            Options {
                urn: urn.ok_or_else(|| anyhow!("a project URN to checkout must be provided"))?,
            },
            vec![],
        ))
    }
}

pub fn run(options: Options) -> anyhow::Result<()> {
    let path = execute(options)?;

    term::headline(&format!(
        "🌱 Project checkout successful under ./{}",
        term::format::highlight(path.file_name().unwrap_or_default().to_string_lossy())
    ));

    Ok(())
}

pub fn execute(options: Options) -> anyhow::Result<PathBuf> {
    let profile = profile::default()?;
    let signer = term::signer(&profile)?;
    let storage = keys::storage(&profile, signer.clone())?;
    let project = project::get(&storage, &options.urn)?
        .context("project could not be found in local storage")?;
    let path = PathBuf::from(project.name.clone());

    if path.exists() {
        anyhow::bail!("the local path {:?} already exists", path.as_path());
    }

    term::headline(&format!(
        "Initializing local checkout for 🌱 {} ({})",
        term::format::highlight(&options.urn),
        project.name,
    ));

    // If we have a local head, we should checkout our local "fork", so we don't specify
    // a peer.
    // If we *don't* have a local head, we have to checkout a delegate's head. If there is
    // only one delegate, the choice is easy.
    let peer = if project::get_local_head(&storage, &options.urn, &project.default_branch)?
        .is_some()
    {
        term::success!("Local {} branch found...", project.default_branch);
        None
    } else if project.remotes.len() > 1 {
        anyhow::bail!("project has more than one delegate, please specify which one you would like to checkout");
    } else if let Some(delegate) = project.remotes.iter().next() {
        term::success!(
            "Remote {} branch found via {}...",
            project.default_branch,
            term::format::highlight(delegate)
        );
        Some(*delegate)
    } else {
        anyhow::bail!("project has no delegates, cannot checkout");
    };

    let spinner = term::spinner("Performing checkout...");
    match project::checkout(
        &storage,
        profile.paths().clone(),
        signer.clone(),
        &options.urn,
        peer,
        path.clone(),
    ) {
        Err(err) => {
            spinner.failed();
            term::blank();

            return Err(err);
        }
        Ok(repo) => {
            spinner.finish();

            // Setup signing.
            if let Err(err) = rad_init::setup_signing(storage.peer_id(), &repo) {
                term::warning(&format!("Warning: Could not setup signing: {:#}", err));
            }

            // Setup a remote and tracking branch for all project delegates except yourself.
            let setup = project::SetupRemote {
                project: &project,
                repo: &repo,
                signer,
                fetch: true,
                upstream: true,
            };
            for peer in &project.remotes {
                if peer != storage.peer_id() {
                    if let Some(upstream) = setup.run(peer, &profile, &storage)? {
                        term::success!(
                            "Remote-tracking branch {} created for {}",
                            term::format::highlight(&upstream),
                            term::format::tertiary(fmt::peer(peer))
                        );
                    }
                }
            }
        }
    }

    Ok(path)
}
