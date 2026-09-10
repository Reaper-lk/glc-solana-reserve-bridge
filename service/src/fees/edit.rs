//! Writing ONE route's fee back into an operator's config file.
//!
//! The same three rules [`crate::chain_policy::edit`] documents, for the
//! same reasons — the file is edited as a DOCUMENT so comments survive,
//! nothing is written until the candidate has been loaded by the real
//! parser, and the bytes that were validated are the bytes installed. The
//! backup-and-rename half is literally that module's
//! [`crate::chain_policy::edit::install`], not a second copy.
//!
//! # The one rule this module adds
//!
//! **Exactly one route's key changes.** A fee edit names a route, and the
//! validation below proves — against the reloaded candidate, not against
//! the document it just built — that every OTHER route's resolved rate is
//! byte-identical to what it was before. That check is the whole point of
//! this module existing rather than the operator editing the table by
//! hand: "change RhnToGlc to 4%" must not be able to become "change
//! RhnToGlc to 4% and, because the table was rewritten, quietly normalise
//! GlcToRhn too".
//!
//! # Writing `[fees]` into a config that has none
//!
//! Every production config file today has no `[fees]` section, and its
//! rates come from the migration fallback in
//! [`crate::config`]. The first edit therefore has to CREATE the section —
//! and creating it makes it authoritative, which means it must be created
//! COMPLETE. So [`plan`] seeds the new table from the rates the config
//! currently resolves to (i.e. exactly what it is already charging) and
//! then applies the one requested change on top. The result is a file
//! that states, explicitly, what the deployment was already doing, with
//! one route moved.
//!
//! That is why [`FeeEditPlan::seeded_routes`] exists: an operator
//! deserves to be told that three other keys appeared, and that they
//! appeared holding the values that were already in force.

use std::fs;
use std::path::{Path, PathBuf};

use toml_edit::{value, DocumentMut};

use super::{executable_routes, FeeError, RouteFees};
use crate::chain_policy::edit::{install, write_file_synced, CommitReport, EditError};
use crate::config::Config;
use crate::routes::Route;

/// A validated, not-yet-installed fee edit.
#[derive(Debug)]
pub struct FeeEditPlan {
    path: PathBuf,
    candidate: PathBuf,
    route: Route,
    before: u64,
    after: u64,
    /// The table as it will be, for display.
    resulting: RouteFees,
    /// Routes whose key this edit had to WRITE OUT even though their rate
    /// is unchanged, because the config had no `[fees]` section and one
    /// cannot be created half-empty. Empty on every subsequent edit.
    seeded_routes: Vec<Route>,
}

impl FeeEditPlan {
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn candidate_path(&self) -> &Path {
        &self.candidate
    }
    pub fn route(&self) -> Route {
        self.route
    }
    /// The rate this route prices at now.
    pub fn before(&self) -> u64 {
        self.before
    }
    /// The rate it would price at.
    pub fn after(&self) -> u64 {
        self.after
    }
    pub fn is_noop(&self) -> bool {
        self.before == self.after
    }
    /// Every route's rate as the edited file resolves them.
    pub fn resulting(&self) -> &RouteFees {
        &self.resulting
    }
    pub fn seeded_routes(&self) -> &[Route] {
        &self.seeded_routes
    }
    pub fn rendered(&self) -> Result<String, EditError> {
        fs::read_to_string(&self.candidate).map_err(|source| EditError::Read {
            path: self.candidate.clone(),
            source,
        })
    }
    /// Removes the candidate without installing it — what a dry run does.
    pub fn discard(self) {
        let _ = fs::remove_file(&self.candidate);
    }
}

/// Plans a one-route fee change.
///
/// Writes ONLY a candidate file beside the target. The target is untouched
/// whatever happens here.
pub fn plan(path: &Path, route: Route, fee_bps: u64) -> Result<FeeEditPlan, EditError> {
    // Refuse a route that cannot be priced before touching the file, so
    // the message names the reason rather than a parse failure later.
    if route.as_direction().is_none() {
        return Err(EditError::CandidateRejected {
            detail: FeeError::RouteNotExecutable {
                route: route.as_str(),
            }
            .to_string(),
        });
    }
    // Validate the RATE the same way the config parser will, so a bad
    // number is refused here rather than by the candidate reload with a
    // less specific message.
    {
        let mut probe = RouteFees::new();
        probe
            .insert(route, fee_bps)
            .map_err(|e| EditError::CandidateRejected {
                detail: e.to_string(),
            })?;
    }

    // BEFORE comes from the real parser: what the file MEANS is what
    // `Config::load` says it means, including the migration fallback for
    // a file with no `[fees]` section at all.
    let existing = Config::load(path).map_err(|e| EditError::CandidateRejected {
        detail: format!("the EXISTING config file does not load: {e}"),
    })?;
    let before = existing
        .route_fees
        .fee_bps(route)
        .map_err(|e| EditError::CandidateRejected {
            detail: e.to_string(),
        })?;

    let text = fs::read_to_string(path).map_err(|source| EditError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let mut doc: DocumentMut = text.parse().map_err(|source| EditError::Parse {
        path: path.to_path_buf(),
        source,
    })?;

    let had_section = doc.get("fees").is_some();
    let fees_table = crate::chain_policy::edit::ensure_table(doc.as_table_mut(), "fees")?;

    // Creating the section makes it authoritative, so it must be created
    // complete — seeded with the rates already in force.
    let mut seeded_routes = Vec::new();
    for other in executable_routes() {
        if other == route {
            continue;
        }
        if fees_table.get(other.as_str()).is_none() {
            let current =
                existing
                    .route_fees
                    .fee_bps(other)
                    .map_err(|e| EditError::CandidateRejected {
                        detail: e.to_string(),
                    })?;
            fees_table[other.as_str()] = value(to_toml_bps(current)?);
            seeded_routes.push(other);
        }
    }
    fees_table[route.as_str()] = value(to_toml_bps(fee_bps)?);
    let _ = had_section;

    let parent = path.parent().ok_or_else(|| EditError::NoParentDirectory {
        path: path.to_path_buf(),
    })?;
    let candidate = parent.join(format!(
        "{}.route-fee-candidate.{}",
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "config.toml".to_string()),
        std::process::id()
    ));
    write_file_synced(&candidate, doc.to_string().as_bytes())?;

    // The proof. Same function the daemon runs at startup.
    let loaded = match Config::load(&candidate) {
        Ok(loaded) => loaded,
        Err(e) => {
            let _ = fs::remove_file(&candidate);
            return Err(EditError::CandidateRejected {
                detail: e.to_string(),
            });
        }
    };

    // The requested change happened...
    match loaded.route_fees.fee_bps(route) {
        Ok(actual) if actual == fee_bps => {}
        other => {
            let _ = fs::remove_file(&candidate);
            return Err(EditError::CandidateRejected {
                detail: format!(
                    "the edited file resolves {} to {:?}, not the requested {fee_bps} bps",
                    route.as_str(),
                    other.map(|bps| bps.to_string()).unwrap_or_default()
                ),
            });
        }
    }
    // ...and NOTHING ELSE did. Checked against the reloaded candidate, so
    // this catches an unrelated route moving for any reason at all —
    // including one this module did not anticipate.
    for other in executable_routes() {
        if other == route {
            continue;
        }
        let was = existing.route_fees.fee_bps(other);
        let now = loaded.route_fees.fee_bps(other);
        if was.as_ref().ok() != now.as_ref().ok() {
            let _ = fs::remove_file(&candidate);
            return Err(EditError::CandidateRejected {
                detail: format!(
                    "editing {} would also change {}'s rate ({:?} -> {:?}). Refusing: a fee edit \
                     changes exactly one route",
                    route.as_str(),
                    other.as_str(),
                    was.ok(),
                    now.ok(),
                ),
            });
        }
    }

    Ok(FeeEditPlan {
        path: path.to_path_buf(),
        candidate,
        route,
        before,
        after: fee_bps,
        resulting: loaded.route_fees,
        seeded_routes,
    })
}

/// Installs a planned edit: timestamped backup, then one atomic rename.
pub fn commit(plan: FeeEditPlan, now_unix: i64) -> Result<CommitReport, EditError> {
    install(&plan.path, &plan.candidate, now_unix)
}

fn to_toml_bps(fee_bps: u64) -> Result<i64, EditError> {
    i64::try_from(fee_bps).map_err(|_| EditError::CandidateRejected {
        detail: format!(
            "fee_bps {fee_bps} is above TOML's signed 64-bit integer range, so it cannot be \
             written to a config file at all"
        ),
    })
}

#[cfg(test)]
mod tests;
