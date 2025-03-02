#![allow(clippy::too_many_arguments)]
use std::collections::{HashMap, HashSet};
use std::convert::{TryFrom, TryInto};
use std::ops::{ControlFlow, RangeInclusive};
use std::str::FromStr;

use automerge::{Automerge, AutomergeError, ObjType, ScalarValue, Value};
use lazy_static::lazy_static;
use nonempty::NonEmpty;
use serde::{Deserialize, Serialize};

use librad::collaborative_objects::{
    CollaborativeObjects, EntryContents, History, NewObjectSpec, ObjectId, TypeName,
};
use librad::git::identities::local::LocalIdentity;
use librad::git::Storage;
use librad::git::Urn;
use librad::paths::Paths;
use librad::PeerId;

use radicle_git_ext as git;

use crate::cobs::shared;
use crate::cobs::shared::*;
use crate::project;

lazy_static! {
    pub static ref TYPENAME: TypeName = FromStr::from_str("xyz.radicle.patch").unwrap();
    pub static ref SCHEMA: serde_json::Value =
        serde_json::from_slice(include_bytes!("patch.json")).unwrap();
}

/// Identifier for a patch.
pub type PatchId = ObjectId;

/// Identifier for a revision.
pub type RevisionId = usize;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Create error: {0}")]
    Create(String),

    #[error("List error: {0}")]
    List(String),

    #[error("Retrieve error: {0}")]
    Retrieve(String),

    #[error(transparent)]
    Automerge(#[from] AutomergeError),
}

#[derive(Debug, Clone, Serialize)]
pub struct Patch {
    /// Author of the patch.
    pub author: Author, // TODO: Should this be plural?
    /// Title of the patch.
    pub title: String,
    /// Current state of the patch.
    pub state: State,
    /// Target branch this patch is meant to be merged in.
    pub target: git::OneLevel,
    /// Labels associated with the patch.
    pub labels: HashSet<Label>,
    /// List of patch revisions. The initial changeset is part of the
    /// first revision.
    pub revisions: NonEmpty<Revision>,
    /// Patch creation time.
    pub timestamp: Timestamp,
}

impl TryFrom<Automerge> for Patch {
    type Error = AutomergeError;

    fn try_from(doc: Automerge) -> Result<Self, Self::Error> {
        let (_obj, obj_id) = doc.get(automerge::ObjId::Root, "patch")?.unwrap();
        let (title, _) = doc.get(&obj_id, "title")?.unwrap();
        let (author, _) = doc.get(&obj_id, "author")?.unwrap();
        let (state, _) = doc.get(&obj_id, "state")?.unwrap();
        let (target, _) = doc.get(&obj_id, "target")?.unwrap();
        let (timestamp, _) = doc.get(&obj_id, "timestamp")?.unwrap();
        let (labels, labels_id) = doc.get(&obj_id, "labels")?.unwrap();

        assert_eq!(labels.to_objtype(), Some(ObjType::Map));

        let mut revisions = Vec::new();
        let (_, revisions_id) = doc.get(&obj_id, "revisions")?.unwrap();
        for i in 0..doc.length(&revisions_id) {
            let revision = lookup::revision(&doc, &revisions_id, i).unwrap();
            revisions.push(revision);
        }

        // Labels.
        let mut labels = HashSet::new();
        for key in doc.keys(&labels_id) {
            let label = Label::new(key).unwrap();

            labels.insert(label);
        }

        let author = shared::author(author)?;
        let state = State::try_from(state).unwrap();
        let revisions = NonEmpty::from_vec(revisions).unwrap();
        let target =
            git::OneLevel::try_from(git::RefLike::try_from(target.to_str().unwrap()).unwrap())
                .unwrap();
        let timestamp = Timestamp::try_from(timestamp).unwrap();

        Ok(Self {
            author,
            title: title.into_string().unwrap(),
            state,
            target,
            labels,
            revisions,
            timestamp,
        })
    }
}

impl TryFrom<&History> for Patch {
    type Error = anyhow::Error;

    fn try_from(history: &History) -> Result<Self, Self::Error> {
        let doc = history.traverse(Automerge::new(), |mut doc, entry| {
            match entry.contents() {
                EntryContents::Automerge(bytes) => {
                    match automerge::Change::from_bytes(bytes.clone()) {
                        Ok(change) => {
                            doc.apply_changes([change]).ok();
                        }
                        Err(_err) => {
                            // Ignore
                        }
                    }
                }
            }
            ControlFlow::Continue(doc)
        });
        let patch = Patch::try_from(doc)?;

        Ok(patch)
    }
}

pub struct Patches<'a> {
    store: CollaborativeObjects<'a>,
    whoami: LocalIdentity,
    peer_id: PeerId,
}

impl<'a> Patches<'a> {
    pub fn new(whoami: LocalIdentity, paths: &Paths, storage: &'a Storage) -> Result<Self, Error> {
        let store = storage.collaborative_objects(Some(paths.cob_cache_dir().to_path_buf()));
        let peer_id = *storage.peer_id();

        Ok(Self {
            store,
            whoami,
            peer_id,
        })
    }

    pub fn create(
        &self,
        project: &Urn,
        title: &str,
        description: &str,
        target: &git::OneLevel,
        commit: &git::Oid,
        labels: &[Label],
    ) -> Result<PatchId, Error> {
        let author = self.whoami.urn();
        let timestamp = Timestamp::now();
        let history = events::create(
            &author,
            &self.peer_id,
            title,
            description,
            target,
            commit,
            timestamp,
            labels,
        )?;

        cobs::create(history, project, &self.whoami, &self.store)
    }

    pub fn get(&self, project: &Urn, id: &PatchId) -> Result<Option<Patch>, Error> {
        let cob = self
            .store
            .retrieve(project, &TYPENAME, id)
            .map_err(|e| Error::Retrieve(e.to_string()))?;

        if let Some(cob) = cob {
            let patch = Patch::try_from(cob.history()).unwrap();
            Ok(Some(patch))
        } else {
            Ok(None)
        }
    }

    pub fn all(&self, project: &Urn) -> Result<Vec<(PatchId, Patch)>, Error> {
        let cobs = self
            .store
            .list(project, &TYPENAME)
            .map_err(|e| Error::List(e.to_string()))?;

        let mut patches = Vec::new();
        for cob in cobs {
            let patch: Result<Patch, _> = cob.history().try_into();
            patches.push((*cob.id(), patch.unwrap()));
        }
        patches.sort_by_key(|(_, p)| p.timestamp);

        Ok(patches)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Open,
    Draft,
    Closed,
    // TODO: Do we need a "rejected", or is it enough to have "closed" with a rejected review?
}

impl From<State> for ScalarValue {
    fn from(state: State) -> Self {
        match state {
            State::Open => ScalarValue::from("open"),
            State::Draft => ScalarValue::from("draft"),
            State::Closed => ScalarValue::from("closed"),
        }
    }
}

impl<'a> TryFrom<Value<'a>> for State {
    type Error = &'static str;

    fn try_from(value: Value) -> Result<Self, Self::Error> {
        let state = value.to_str().ok_or("value isn't a string")?;

        match state {
            "open" => Ok(Self::Open),
            "draft" => Ok(Self::Draft),
            "closed" => Ok(Self::Closed),
            _ => Err("invalid state name"),
        }
    }
}

/// A patch revision.
#[derive(Debug, Clone, Serialize)]
pub struct Revision {
    /// Author of this revision.
    /// Note that this doesn't have to match the author of the patch.
    pub author: Author,
    /// Peer who published this revision.
    pub peer: PeerId,
    /// Patch revision number.
    pub version: usize,
    /// Reference to the Git object containing the code.
    pub commit: git::Oid,
    /// "Cover letter" for this changeset.
    pub comment: Comment,
    /// Discussion around this revision.
    pub discussion: Discussion,
    /// Reviews (one per user) of the changes.
    pub reviews: HashMap<Urn, Review>,
    /// Merges of this revision into other repositories.
    pub merges: Vec<Merge>,
    /// When this revision was created.
    pub timestamp: Timestamp,
}

/// A merged patch revision.
#[derive(Debug, Clone, Serialize)]
pub struct Merge {
    /// Peer information of repository that this patch was merged into.
    pub peer: project::PeerInfo,
    /// Revision that was merged.
    pub revision: RevisionId,
    /// Base branch commit that contains the revision.
    pub commit: git::Oid,
    /// When this merged was performed.
    pub timestamp: Timestamp,
}

/// A patch review verdict.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// Accept patch.
    Accept,
    /// Reject patch.
    Reject,
    /// Don't give a verdict.
    Pass,
}

impl From<Verdict> for ScalarValue {
    fn from(verdict: Verdict) -> Self {
        let s = serde_json::to_string(&verdict).unwrap(); // Cannot fail.
        ScalarValue::from(s)
    }
}

impl<'a> TryFrom<Value<'a>> for Verdict {
    type Error = serde_json::Error;

    fn try_from(value: Value) -> Result<Self, Self::Error> {
        let verdict = value
            .to_str()
            .ok_or(serde::de::Error::custom("value is not a string"))?;
        serde_json::from_str(verdict)
    }
}

/// Code location, used for attaching comments.
#[derive(Debug, Clone, Serialize)]
pub struct CodeLocation {
    /// Line number commented on.
    pub lines: RangeInclusive<usize>,
    /// Commit commented on.
    pub commit: git::Oid,
    /// File being commented on.
    pub blob: git::Oid,
}

/// Comment on code.
#[derive(Debug, Clone, Serialize)]
pub struct CodeComment {
    /// Code location of the comment.
    location: CodeLocation,
    /// Comment.
    comment: Comment,
}

/// A patch review on a revision.
#[derive(Debug, Clone, Serialize)]
pub struct Review {
    /// Review author.
    pub author: Author,
    /// Review verdict.
    pub verdict: Verdict,
    /// Review general comment.
    pub comment: Comment,
    /// Review inline code comments.
    pub inline: Vec<CodeComment>,
    /// Review timestamp.
    pub timestamp: Timestamp,
}

mod lookup {
    use super::*;

    pub fn revision(
        doc: &Automerge,
        revisions_id: &automerge::ObjId,
        ix: usize,
    ) -> Result<Revision, AutomergeError> {
        let (_, revision_id) = doc.get(&revisions_id, ix)?.unwrap();
        let (_, comment_id) = doc.get(&revision_id, "comment")?.unwrap();
        let (_, discussion_id) = doc.get(&revision_id, "discussion")?.unwrap();
        let (_, _reviews_id) = doc.get(&revision_id, "reviews")?.unwrap();
        let (_, _merges_id) = doc.get(&revision_id, "merges")?.unwrap();
        let (author, _) = doc.get(&revision_id, "author")?.unwrap();
        let (peer, _) = doc.get(&revision_id, "peer")?.unwrap();
        let (commit, _) = doc.get(&revision_id, "commit")?.unwrap();
        let (version, _) = doc.get(&revision_id, "version")?.unwrap();
        let (timestamp, _) = doc.get(&revision_id, "timestamp")?.unwrap();

        // Top-level comment.
        let comment = shared::lookup::comment(doc, &comment_id)?;

        // Discussion thread.
        let mut discussion: Discussion = Vec::new();
        for i in 0..doc.length(&discussion_id) {
            let (_, comment_id) = doc.get(&discussion_id, i as usize)?.unwrap();
            let comment = shared::lookup::thread(doc, &comment_id)?;

            discussion.push(comment);
        }

        let author = lookup::author(author)?;
        let peer = PeerId::from_str(peer.to_str().unwrap()).unwrap();
        let version = version.to_u64().unwrap() as usize;
        let commit = commit.to_str().unwrap().try_into().unwrap();
        let reviews = HashMap::new();
        let merges = Vec::new();
        let timestamp = Timestamp::try_from(timestamp).unwrap();

        assert_eq!(version, ix);

        Ok(Revision {
            author,
            peer,
            version,
            commit,
            comment,
            discussion,
            reviews,
            merges,
            timestamp,
        })
    }
}

mod cobs {
    use super::*;

    pub(super) fn create(
        history: EntryContents,
        project: &Urn,
        whoami: &LocalIdentity,
        store: &CollaborativeObjects,
    ) -> Result<PatchId, Error> {
        let cob = store
            .create(
                whoami,
                project,
                NewObjectSpec {
                    schema_json: SCHEMA.clone(),
                    typename: TYPENAME.clone(),
                    message: Some("Create issue".to_owned()),
                    history,
                },
            )
            .map_err(|e| Error::Create(e.to_string()))?;

        Ok(*cob.id())
    }
}

mod events {
    use super::*;
    use automerge::{
        transaction::{CommitOptions, Transactable},
        ObjId,
    };

    pub fn create(
        author: &Urn,
        peer: &PeerId,
        title: &str,
        description: &str,
        target: &git::OneLevel,
        commit: &git::Oid,
        timestamp: Timestamp,
        labels: &[Label],
    ) -> Result<EntryContents, AutomergeError> {
        let title = title.trim();
        // TODO: Return error.
        if title.is_empty() {
            panic!("Empty patch title");
        }

        let mut doc = Automerge::new();
        let _issue = doc
            .transact_with::<_, _, AutomergeError, _, ()>(
                |_| CommitOptions::default().with_message("Create patch".to_owned()),
                |tx| {
                    let patch_id = tx.put_object(ObjId::Root, "patch", ObjType::Map)?;

                    tx.put(&patch_id, "title", title)?;
                    tx.put(&patch_id, "author", author.to_string())?;
                    tx.put(&patch_id, "state", State::Open)?;
                    tx.put(&patch_id, "target", target.to_string())?;
                    tx.put(&patch_id, "timestamp", timestamp)?;

                    let labels_id = tx.put_object(&patch_id, "labels", ObjType::Map)?;
                    for label in labels {
                        tx.put(&labels_id, label.name().trim(), true)?;
                    }

                    let revisions_id = tx.put_object(&patch_id, "revisions", ObjType::List)?;
                    {
                        let revision_id = tx.insert_object(&revisions_id, 0, ObjType::Map)?;

                        tx.put(&revision_id, "author", author.to_string())?;
                        tx.put(&revision_id, "peer", peer.to_string())?;
                        tx.put(&revision_id, "version", 0)?;
                        tx.put(&revision_id, "commit", commit.to_string())?;
                        {
                            // Top-level comment for first patch revision.
                            // Nb. top-level comment doesn't have a `replies` field.
                            let comment_id =
                                tx.put_object(&revision_id, "comment", ObjType::Map)?;

                            tx.put(&comment_id, "body", description.trim())?;
                            tx.put(&comment_id, "author", author.to_string())?;
                            tx.put(&comment_id, "timestamp", timestamp)?;
                            tx.put_object(&comment_id, "reactions", ObjType::Map)?;
                        }
                        tx.put_object(&revision_id, "discussion", ObjType::List)?;
                        tx.put_object(&revision_id, "reviews", ObjType::Map)?;
                        tx.put_object(&revision_id, "merges", ObjType::List)?;
                        tx.put(&revision_id, "timestamp", timestamp)?;
                    }

                    Ok(patch_id)
                },
            )
            .map_err(|failure| failure.error)?
            .result;

        Ok(EntryContents::Automerge(doc.save_incremental()))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::test;

    #[test]
    fn test_patch_create_and_get() {
        let (storage, profile, whoami, project) = test::setup::profile();
        let author = whoami.urn();
        let patches = Patches::new(whoami, profile.paths(), &storage).unwrap();
        let target = git::OneLevel::try_from(git::RefLike::try_from("master").unwrap()).unwrap();
        let commit = git::Oid::from(git2::Oid::zero());
        let patch_id = patches
            .create(
                &project.urn(),
                "My first patch",
                "Blah blah blah.",
                &target,
                &commit,
                &[],
            )
            .unwrap();
        let patch = patches.get(&project.urn(), &patch_id).unwrap().unwrap();
        let timestamp = Timestamp::now();

        assert_eq!(&patch.title, "My first patch");
        assert_eq!(patch.author.urn(), &author);
        assert_eq!(patch.state, State::Open);
        assert!(patch.timestamp >= timestamp);

        let revision = patch.revisions.head;

        assert_eq!(revision.author, Author::Urn { urn: author });
        assert_eq!(revision.peer, *storage.peer_id());
        assert_eq!(revision.comment.body, "Blah blah blah.");
        assert_eq!(revision.discussion.len(), 0);
        assert_eq!(revision.version, 0);
        assert_eq!(revision.commit, commit);
        assert!(revision.reviews.is_empty());
        assert!(revision.merges.is_empty());
    }
}
