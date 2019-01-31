// Copyright (c) 2004-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

//! This crate contains the core structs and traits that implement the hook subsystem in
//! Mononoke.
//! Hooks are user defined pieces of code, typically written in a scripting language that
//! can be run at different stages of the process of rebasing user changes into a server side
//! bookmark.
//! The scripting language specific implementation of hooks are in the corresponding sub module.

#![deny(warnings)]
#![feature(try_from)]

extern crate aclchecker;
#[cfg(test)]
#[macro_use]
extern crate assert_matches;
#[cfg(test)]
extern crate async_unit;
extern crate asyncmemo;
extern crate blob_changeset;
extern crate bookmarks;
extern crate bytes;
#[macro_use]
extern crate cloned;
#[macro_use]
extern crate failure_ext as failure;
#[cfg(test)]
extern crate fixtures;
extern crate futures;
#[macro_use]
extern crate futures_ext;
extern crate hlua;
extern crate hlua_futures;
#[macro_use]
extern crate lazy_static;
extern crate linked_hash_map;
#[macro_use]
extern crate maplit;
extern crate mercurial_types;
extern crate metaconfig_types;
extern crate mononoke_types;
#[cfg(test)]
#[macro_use]
extern crate pretty_assertions;
extern crate regex;
#[macro_use]
extern crate slog;
extern crate tempdir;

extern crate context;
extern crate srclient;
extern crate thrift;

pub mod errors;
mod facebook;
pub mod hook_loader;
pub mod lua_hook;
pub mod rust_hook;

use aclchecker::{AclChecker, Identity};
use asyncmemo::{Asyncmemo, Filler, Weight};
use blob_changeset::HgBlobChangeset;
use bookmarks::Bookmark;
use bytes::Bytes;
use context::CoreContext;
pub use errors::*;
use failure::{Error, FutureFailureErrorExt};
use futures::{failed, finished, Future, IntoFuture};
use futures_ext::{BoxFuture, FutureExt};
use mercurial_types::{manifest_utils::EntryStatus, Changeset, HgChangesetId, HgParents, MPath};
use metaconfig_types::{HookBypass, HookConfig, HookManagerParams};
use mononoke_types::FileType;
use slog::Logger;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::mem;
use std::str;
use std::sync::{Arc, Mutex};

type ChangesetHooks = HashMap<String, (Arc<Hook<HookChangeset>>, HookConfig)>;
type FileHooks = Arc<Mutex<HashMap<String, (Arc<Hook<HookFile>>, HookConfig)>>>;
type Cache = Asyncmemo<HookCacheFiller>;

/// Manages hooks and allows them to be installed and uninstalled given a name
/// Knows how to run hooks

pub struct HookManager {
    cache: Cache,
    changeset_hooks: ChangesetHooks,
    file_hooks: FileHooks,
    bookmark_hooks: HashMap<Bookmark, Vec<String>>,
    repo_name: String,
    changeset_store: Box<ChangesetStore>,
    content_store: Arc<FileContentStore>,
    logger: Logger,
    reviewers_acl_checker: Arc<Option<AclChecker>>,
}

impl HookManager {
    pub fn new(
        ctx: CoreContext,
        repo_name: String,
        changeset_store: Box<ChangesetStore>,
        content_store: Arc<FileContentStore>,
        hook_manager_params: HookManagerParams,
        logger: Logger,
    ) -> HookManager {
        let changeset_hooks = HashMap::new();
        let file_hooks = Arc::new(Mutex::new(HashMap::new()));

        let filler = HookCacheFiller {
            ctx,
            file_hooks: file_hooks.clone(),
            repo_name: repo_name.clone(),
        };
        let cache = Asyncmemo::with_limits(
            "hooks",
            filler,
            hook_manager_params.entrylimit,
            hook_manager_params.weightlimit,
        );

        let reviewers_acl_checker = if !hook_manager_params.disable_acl_checker {
            // This can block, but not too big a deal as we create hook manager in server startup
            let reviewers_acl_checker = AclChecker::new(&Identity::from_groupname(
                facebook::REVIEWERS_ACL_GROUP_NAME,
            ));
            let updated = reviewers_acl_checker.do_wait_updated(10000);
            if updated {
                Some(reviewers_acl_checker)
            } else {
                None
            }
        } else {
            None
        };

        HookManager {
            cache,
            changeset_hooks,
            file_hooks,
            bookmark_hooks: HashMap::new(),
            repo_name,
            changeset_store,
            content_store,
            logger,
            reviewers_acl_checker: Arc::new(reviewers_acl_checker),
        }
    }

    pub fn register_changeset_hook(
        &mut self,
        hook_name: &str,
        hook: Arc<Hook<HookChangeset>>,
        config: HookConfig,
    ) {
        self.changeset_hooks
            .insert(hook_name.to_string(), (hook, config));
    }

    pub fn register_file_hook(
        &mut self,
        hook_name: &str,
        hook: Arc<Hook<HookFile>>,
        config: HookConfig,
    ) {
        let mut hooks = self.file_hooks.lock().unwrap();
        hooks.insert(hook_name.to_string(), (hook, config));
    }

    pub fn set_hooks_for_bookmark(&mut self, bookmark: Bookmark, hooks: Vec<String>) {
        self.bookmark_hooks.insert(bookmark, hooks);
    }

    pub fn changeset_hook_names(&self) -> HashSet<String> {
        self.changeset_hooks
            .iter()
            .map(|(name, _)| name.clone())
            .collect()
    }

    pub fn file_hook_names(&self) -> HashSet<String> {
        self.file_hooks
            .lock()
            .unwrap()
            .iter()
            .map(|(name, _)| name.clone())
            .collect()
    }

    // Changeset hooks

    pub fn run_changeset_hooks_for_bookmark(
        &self,
        ctx: CoreContext,
        changeset_id: HgChangesetId,
        bookmark: &Bookmark,
        maybe_pushvars: Option<HashMap<String, Bytes>>,
    ) -> BoxFuture<Vec<(ChangesetHookExecutionID, HookExecution)>, Error> {
        match self.bookmark_hooks.get(bookmark) {
            Some(hooks) => {
                let hooks = hooks
                    .clone()
                    .into_iter()
                    .filter(|name| self.changeset_hooks.contains_key(name))
                    .collect();
                self.run_changeset_hooks_for_changeset_id(ctx, changeset_id, hooks, maybe_pushvars)
            }
            None => return finished(Vec::new()).boxify(),
        }
    }

    fn run_changeset_hooks_for_changeset_id(
        &self,
        ctx: CoreContext,
        changeset_id: HgChangesetId,
        hooks: Vec<String>,
        maybe_pushvars: Option<HashMap<String, Bytes>>,
    ) -> BoxFuture<Vec<(ChangesetHookExecutionID, HookExecution)>, Error> {
        let hooks: Result<Vec<(String, (Arc<Hook<HookChangeset>>, _))>, Error> = hooks
            .iter()
            .map(|hook_name| {
                let hook = self.changeset_hooks
                    .get(hook_name)
                    .ok_or(ErrorKind::NoSuchHook(hook_name.to_string()))?;
                Ok((hook_name.clone(), hook.clone()))
            })
            .collect();
        let hooks = try_boxfuture!(hooks);
        let repo_name = self.repo_name.clone();
        self.get_hook_changeset(ctx.clone(), changeset_id)
            .and_then({
                move |hcs| {
                    let hooks = HookManager::filter_bypassed_hooks(
                        hooks,
                        &hcs.comments,
                        maybe_pushvars.as_ref(),
                    );

                    HookManager::run_changeset_hooks_for_changeset(
                        ctx,
                        repo_name,
                        hcs.clone(),
                        hooks.clone(),
                    )
                }
            })
            .map(move |res| {
                res.into_iter()
                    .map(|(hook_name, exec)| {
                        (
                            ChangesetHookExecutionID {
                                cs_id: changeset_id,
                                hook_name,
                            },
                            exec,
                        )
                    })
                    .collect()
            })
            .boxify()
    }

    fn run_changeset_hooks_for_changeset(
        ctx: CoreContext,
        repo_name: String,
        changeset: HookChangeset,
        hooks: Vec<(String, Arc<Hook<HookChangeset>>, HookConfig)>,
    ) -> BoxFuture<Vec<(String, HookExecution)>, Error> {
        let v: Vec<BoxFuture<(String, HookExecution), _>> = hooks
            .iter()
            .map(move |(hook_name, hook, config)| {
                let hook_context: HookContext<HookChangeset> = HookContext::new(
                    hook_name.clone(),
                    repo_name.clone(),
                    config.clone(),
                    changeset.clone(),
                );
                HookManager::run_changeset_hook(ctx.clone(), hook.clone(), hook_context)
            })
            .collect();
        futures::future::join_all(v).boxify()
    }

    fn run_changeset_hook(
        ctx: CoreContext,
        hook: Arc<Hook<HookChangeset>>,
        hook_context: HookContext<HookChangeset>,
    ) -> BoxFuture<(String, HookExecution), Error> {
        let hook_name = hook_context.hook_name.clone();
        hook.run(ctx, hook_context)
            .map({
                cloned!(hook_name);
                move |he| (hook_name, he)
            })
            .with_context(move |_| format!("while executing hook {}", hook_name))
            .from_err()
            .boxify()
    }

    // File hooks

    pub fn run_file_hooks_for_bookmark(
        &self,
        ctx: CoreContext,
        changeset_id: HgChangesetId,
        bookmark: &Bookmark,
        maybe_pushvars: Option<HashMap<String, Bytes>>,
    ) -> BoxFuture<Vec<(FileHookExecutionID, HookExecution)>, Error> {
        debug!(
            self.logger.clone(),
            "Running file hooks for bookmark {:?}",
            bookmark
        );
        match self.bookmark_hooks.get(bookmark) {
            Some(hooks) => {
                let file_hooks = self.file_hooks.lock().unwrap();
                let hooks = hooks
                    .clone()
                    .into_iter()
                    .filter_map(|name| file_hooks.get(&name).map(|hook| (name, hook.clone())))
                    .collect();
                self.run_file_hooks_for_changeset_id(
                    ctx,
                    changeset_id,
                    hooks,
                    maybe_pushvars,
                    self.logger.clone(),
                )
            }
            None => return Box::new(finished(Vec::new())),
        }
    }

    fn run_file_hooks_for_changeset_id(
        &self,
        ctx: CoreContext,
        changeset_id: HgChangesetId,
        hooks: Vec<(String, (Arc<Hook<HookFile>>, HookConfig))>,
        maybe_pushvars: Option<HashMap<String, Bytes>>,
        logger: Logger,
    ) -> BoxFuture<Vec<(FileHookExecutionID, HookExecution)>, Error> {
        debug!(
            self.logger,
            "Running file hooks for changeset id {:?}", changeset_id
        );
        let cache = self.cache.clone();
        self.get_hook_changeset(ctx.clone(), changeset_id)
            .and_then(move |hcs| {
                let hooks = HookManager::filter_bypassed_hooks(
                    hooks.clone(),
                    &hcs.comments,
                    maybe_pushvars.as_ref(),
                );
                let hooks = hooks.into_iter().map(|(name, _, _)| name).collect();

                HookManager::run_file_hooks_for_changeset(
                    changeset_id,
                    hcs.clone(),
                    hooks,
                    cache,
                    logger,
                )
            })
            .boxify()
    }

    fn run_file_hooks_for_changeset(
        changeset_id: HgChangesetId,
        changeset: HookChangeset,
        hooks: Vec<String>,
        cache: Cache,
        logger: Logger,
    ) -> BoxFuture<Vec<(FileHookExecutionID, HookExecution)>, Error> {
        let v: Vec<BoxFuture<Vec<(FileHookExecutionID, HookExecution)>, _>> = changeset
            .files
            .iter()
            // Do not run file hooks for deleted files
            .filter_map(move |file| {
                match file.ty {
                    ChangedFileType::Added | ChangedFileType::Modified => Some(
                        HookManager::run_file_hooks(
                            changeset_id,
                            file.clone(),
                            hooks.clone(),
                            cache.clone(),
                            logger.clone(),
                        )
                    ),
                    ChangedFileType::Deleted => None,
                }
            })
            .collect();
        futures::future::join_all(v)
            .map(|vv| vv.into_iter().flatten().collect())
            .boxify()
    }

    fn run_file_hooks(
        cs_id: HgChangesetId,
        file: HookFile,
        hooks: Vec<String>,
        cache: Cache,
        logger: Logger,
    ) -> BoxFuture<Vec<(FileHookExecutionID, HookExecution)>, Error> {
        let v: Vec<BoxFuture<(FileHookExecutionID, HookExecution), _>> = hooks
            .iter()
            .map(move |hook_name| {
                HookManager::run_file_hook(
                    FileHookExecutionID {
                        cs_id,
                        hook_name: hook_name.to_string(),
                        file: file.clone(),
                    },
                    cache.clone(),
                    logger.clone(),
                )
            })
            .collect();
        futures::future::join_all(v).boxify()
    }

    fn run_file_hook(
        key: FileHookExecutionID,
        cache: Cache,
        logger: Logger,
    ) -> BoxFuture<(FileHookExecutionID, HookExecution), Error> {
        debug!(logger, "Running file hook {:?}", key);
        let hook_name = key.hook_name.clone();
        cache
            .get(key.clone())
            .map(|he| (key, he))
            .with_context(move |_| format!("while executing hook {}", hook_name))
            .from_err()
            .boxify()
    }

    fn get_hook_changeset(
        &self,
        ctx: CoreContext,
        changeset_id: HgChangesetId,
    ) -> BoxFuture<HookChangeset, Error> {
        let content_store = self.content_store.clone();
        let hg_changeset = self.changeset_store
            .get_changeset_by_changesetid(ctx.clone(), &changeset_id);
        let changed_files = self.changeset_store.get_changed_files(ctx, &changeset_id);
        let reviewers_acl_checker = self.reviewers_acl_checker.clone();
        Box::new((hg_changeset, changed_files).into_future().and_then(
            move |(changeset, changed_files)| {
                let author = str::from_utf8(changeset.user())?.into();
                let files = changed_files
                    .into_iter()
                    .map(|(path, ty)| {
                        HookFile::new(path, content_store.clone(), changeset_id.clone(), ty)
                    })
                    .collect();
                let comments = str::from_utf8(changeset.comments())?.into();
                let parents = HookChangesetParents::from(changeset.parents());
                Ok(HookChangeset::new(
                    author,
                    files,
                    comments,
                    parents,
                    changeset_id,
                    content_store,
                    reviewers_acl_checker,
                ))
            },
        ))
    }

    fn filter_bypassed_hooks<T: Clone>(
        hooks: Vec<(String, (T, HookConfig))>,
        commit_msg: &String,
        maybe_pushvars: Option<&HashMap<String, Bytes>>,
    ) -> Vec<(String, T, HookConfig)> {
        hooks
            .clone()
            .into_iter()
            .filter_map(|(hook_name, (hook, config))| {
                let maybe_bypassed_hook = match config.bypass {
                    Some(ref bypass) => {
                        if HookManager::is_hook_bypassed(bypass, commit_msg, maybe_pushvars) {
                            None
                        } else {
                            Some(())
                        }
                    }
                    None => Some(()),
                };
                maybe_bypassed_hook.map(move |()| (hook_name, hook, config))
            })
            .collect()
    }

    fn is_hook_bypassed(
        bypass: &HookBypass,
        cs_msg: &String,
        maybe_pushvars: Option<&HashMap<String, Bytes>>,
    ) -> bool {
        match bypass {
            HookBypass::CommitMessage(bypass_string) => cs_msg.contains(bypass_string),
            HookBypass::Pushvar { name, value } => {
                if let Some(pushvars) = maybe_pushvars {
                    let pushvar_val = pushvars
                        .get(name)
                        .map(|bytes| String::from_utf8(bytes.to_vec()));

                    if let Some(Ok(pushvar_val)) = pushvar_val {
                        return &pushvar_val == value;
                    }
                    return false;
                }
                return false;
            }
        }
    }
}

pub trait Hook<T>: Send + Sync
where
    T: Clone,
{
    fn run(
        &self,
        ctx: CoreContext,
        hook_context: HookContext<T>,
    ) -> BoxFuture<HookExecution, Error>;
}

/// Represents a changeset - more user friendly than the blob changeset
/// as this uses String not Vec[u8]
#[derive(Clone)]
pub struct HookChangeset {
    pub author: String,
    pub files: Vec<HookFile>,
    pub comments: String,
    pub parents: HookChangesetParents,
    content_store: Arc<FileContentStore>,
    changeset_id: HgChangesetId,
    reviewers_acl_checker: Arc<Option<AclChecker>>,
}

impl fmt::Debug for HookChangeset {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "HookChangeset changeset_id: {:?} files: {:?}, comments: {:?}",
            self.changeset_id, self.files, self.comments
        )
    }
}

impl PartialEq for HookChangeset {
    fn eq(&self, other: &HookChangeset) -> bool {
        self.changeset_id == other.changeset_id
    }
}

#[derive(Clone)]
pub enum ChangedFileType {
    Added,
    Deleted,
    Modified,
}

impl From<EntryStatus> for ChangedFileType {
    fn from(entry_status: EntryStatus) -> Self {
        match entry_status {
            EntryStatus::Added(_) => ChangedFileType::Added,
            EntryStatus::Deleted(_) => ChangedFileType::Deleted,
            EntryStatus::Modified { .. } => ChangedFileType::Modified,
        }
    }
}

#[derive(Clone)]
pub struct HookFile {
    pub path: String,
    content_store: Arc<FileContentStore>,
    changeset_id: HgChangesetId,
    ty: ChangedFileType,
}

impl fmt::Debug for HookFile {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "HookFile path: {}, changeset_id: {}",
            self.path, self.changeset_id
        )
    }
}

impl PartialEq for HookFile {
    fn eq(&self, other: &HookFile) -> bool {
        self.path == other.path && self.changeset_id == other.changeset_id
    }
}

impl Weight for HookFile {
    fn get_weight(&self) -> usize {
        self.path.get_weight() + self.changeset_id.get_weight()
    }
}

impl Eq for HookFile {}

impl Hash for HookFile {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.path.hash(state);
        self.changeset_id.hash(state);
    }
}

impl HookFile {
    pub fn new(
        path: String,
        content_store: Arc<FileContentStore>,
        changeset_id: HgChangesetId,
        ty: ChangedFileType,
    ) -> HookFile {
        HookFile {
            path,
            content_store,
            changeset_id,
            ty,
        }
    }

    pub fn contains_string(&self, ctx: CoreContext, data: &str) -> BoxFuture<bool, Error> {
        let data = data.to_string();
        self.file_content(ctx)
            .and_then(move |bytes| {
                let str_content = str::from_utf8(&bytes)?.to_string();
                Ok(str_content.contains(&data))
            })
            .boxify()
    }

    pub fn len(&self, ctx: CoreContext) -> BoxFuture<u64, Error> {
        self.file_content(ctx)
            .and_then(|bytes| Ok(bytes.len() as u64))
            .boxify()
    }

    pub fn file_content(&self, ctx: CoreContext) -> BoxFuture<Bytes, Error> {
        let path = try_boxfuture!(MPath::new(self.path.as_bytes()));
        let changeset_id = self.changeset_id.clone();
        self.content_store
            .get_file_content_for_changeset(ctx, self.changeset_id, path.clone())
            .and_then(move |opt| {
                opt.ok_or(ErrorKind::NoFileContent(changeset_id, path.into()).into())
            })
            .map(|(_, bytes)| bytes)
            .boxify()
    }

    pub fn file_type(&self, ctx: CoreContext) -> BoxFuture<FileType, Error> {
        let path = try_boxfuture!(MPath::new(self.path.as_bytes()));
        let changeset_id = self.changeset_id.clone();
        self.content_store
            .get_file_content_for_changeset(ctx, self.changeset_id, path.clone())
            .and_then(move |opt| {
                opt.ok_or(ErrorKind::NoFileContent(changeset_id, path.into()).into())
            })
            .map(|(file_type, _)| file_type)
            .boxify()
    }
}

impl HookChangeset {
    pub fn new(
        author: String,
        files: Vec<HookFile>,
        comments: String,
        parents: HookChangesetParents,
        changeset_id: HgChangesetId,
        content_store: Arc<FileContentStore>,
        reviewers_acl_checker: Arc<Option<AclChecker>>,
    ) -> HookChangeset {
        HookChangeset {
            author,
            files,
            comments,
            parents,
            content_store,
            changeset_id,
            reviewers_acl_checker,
        }
    }

    pub fn file_content(&self, ctx: CoreContext, path: String) -> BoxFuture<Option<Bytes>, Error> {
        let path = try_boxfuture!(MPath::new(path.as_bytes()));
        self.content_store
            .get_file_content_for_changeset(ctx, self.changeset_id, path.clone())
            .map(|opt| opt.map(|(_, bytes)| bytes))
            .boxify()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum HookExecution {
    Accepted,
    Rejected(HookRejectionInfo),
}

impl Weight for HookExecution {
    fn get_weight(&self) -> usize {
        match self {
            HookExecution::Accepted => mem::size_of::<Self>(),
            HookExecution::Rejected(info) => mem::size_of::<Self>() + info.get_weight(),
        }
    }
}

/// Information on why the hook rejected the changeset
#[derive(Clone, Debug, PartialEq)]
pub struct HookRejectionInfo {
    pub description: String,
    pub long_description: String,
}

impl Weight for HookRejectionInfo {
    fn get_weight(&self) -> usize {
        mem::size_of::<Self>() + self.description.get_weight() + self.long_description.get_weight()
    }
}

impl HookRejectionInfo {
    pub fn new(description: String, long_description: String) -> HookRejectionInfo {
        HookRejectionInfo {
            description,
            long_description,
        }
    }
}

pub trait ChangesetStore: Send + Sync {
    fn get_changeset_by_changesetid(
        &self,
        ctx: CoreContext,
        changesetid: &HgChangesetId,
    ) -> BoxFuture<HgBlobChangeset, Error>;

    fn get_changed_files(
        &self,
        ctx: CoreContext,
        changesetid: &HgChangesetId,
    ) -> BoxFuture<Vec<(String, ChangedFileType)>, Error>;
}

pub struct InMemoryChangesetStore {
    map: HashMap<HgChangesetId, HgBlobChangeset>,
}

impl ChangesetStore for InMemoryChangesetStore {
    fn get_changeset_by_changesetid(
        &self,
        _ctx: CoreContext,
        changesetid: &HgChangesetId,
    ) -> BoxFuture<HgBlobChangeset, Error> {
        match self.map.get(changesetid) {
            Some(cs) => Box::new(finished(cs.clone())),
            None => Box::new(failed(
                ErrorKind::NoSuchChangeset(changesetid.to_string()).into(),
            )),
        }
    }

    fn get_changed_files(
        &self,
        _ctx: CoreContext,
        changesetid: &HgChangesetId,
    ) -> BoxFuture<Vec<(String, ChangedFileType)>, Error> {
        match self.map.get(changesetid) {
            Some(cs) => Box::new(finished(
                cs.files()
                    .into_iter()
                    .map(|arr| String::from_utf8_lossy(&arr.to_vec()).into_owned())
                    .map(|path| (path, ChangedFileType::Added))
                    .collect(),
            )),
            None => Box::new(failed(
                ErrorKind::NoSuchChangeset(changesetid.to_string()).into(),
            )),
        }
    }
}

impl InMemoryChangesetStore {
    pub fn new() -> InMemoryChangesetStore {
        InMemoryChangesetStore {
            map: HashMap::new(),
        }
    }

    pub fn insert(&mut self, changeset_id: &HgChangesetId, changeset: &HgBlobChangeset) {
        self.map.insert(changeset_id.clone(), changeset.clone());
    }
}

pub trait FileContentStore: Send + Sync {
    fn get_file_content_for_changeset(
        &self,
        ctx: CoreContext,
        changesetid: HgChangesetId,
        path: MPath,
    ) -> BoxFuture<Option<(FileType, Bytes)>, Error>;
}

#[derive(Clone)]
pub struct InMemoryFileContentStore {
    map: HashMap<(HgChangesetId, MPath), (FileType, Bytes)>,
}

impl FileContentStore for InMemoryFileContentStore {
    fn get_file_content_for_changeset(
        &self,
        _ctx: CoreContext,
        changesetid: HgChangesetId,
        path: MPath,
    ) -> BoxFuture<Option<(FileType, Bytes)>, Error> {
        let opt = self.map
            .get(&(changesetid, path.clone()))
            .map(|(file_type, bytes)| (file_type.clone(), bytes.clone()));
        finished(opt).boxify()
    }
}

impl InMemoryFileContentStore {
    pub fn new() -> InMemoryFileContentStore {
        InMemoryFileContentStore {
            map: HashMap::new(),
        }
    }

    pub fn insert(&mut self, key: (HgChangesetId, MPath), content: (FileType, Bytes)) {
        self.map.insert(key, content);
    }
}

struct HookCacheFiller {
    ctx: CoreContext,
    repo_name: String,
    file_hooks: FileHooks,
}

impl Filler for HookCacheFiller {
    type Key = FileHookExecutionID;
    type Value = BoxFuture<HookExecution, Error>;

    fn fill(&self, _cache: &Asyncmemo<Self>, key: &Self::Key) -> Self::Value {
        let hooks = self.file_hooks.lock().unwrap();
        match hooks.get(&key.hook_name) {
            Some(arc_hook) => {
                let arc_hook = arc_hook.clone();
                let hook_context: HookContext<HookFile> = HookContext::new(
                    key.hook_name.clone(),
                    self.repo_name.clone(),
                    arc_hook.1.clone(),
                    key.file.clone(),
                );
                arc_hook.0.run(self.ctx.clone(), hook_context)
            }
            None => panic!("Can't find hook {}", key.hook_name), // TODO
        }
    }
}

#[derive(Clone, Debug, PartialEq, Hash, Eq)]
// TODO Note that when we move to Bonsai changesets the ID that we use in the cache will
// be the content hash
pub struct FileHookExecutionID {
    pub cs_id: HgChangesetId,
    pub hook_name: String,
    pub file: HookFile,
}

#[derive(Clone, Debug, PartialEq, Hash, Eq)]
pub struct ChangesetHookExecutionID {
    pub cs_id: HgChangesetId,
    pub hook_name: String,
}

impl Weight for FileHookExecutionID {
    fn get_weight(&self) -> usize {
        self.cs_id.get_weight() + self.hook_name.get_weight() + self.file.get_weight()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum HookChangesetParents {
    None,
    One(String),
    Two(String, String),
}

impl From<HgParents> for HookChangesetParents {
    fn from(parents: HgParents) -> Self {
        match parents {
            HgParents::None => HookChangesetParents::None,
            HgParents::One(p1_hash) => HookChangesetParents::One(p1_hash.to_string()),
            HgParents::Two(p1_hash, p2_hash) => {
                HookChangesetParents::Two(p1_hash.to_string(), p2_hash.to_string())
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct HookContext<T>
where
    T: Clone,
{
    pub hook_name: String,
    pub repo_name: String,
    pub config: HookConfig,
    pub data: T,
}

impl<T> HookContext<T>
where
    T: Clone,
{
    fn new(hook_name: String, repo_name: String, config: HookConfig, data: T) -> HookContext<T> {
        HookContext {
            hook_name,
            repo_name,
            config,
            data,
        }
    }
}
