/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::io::Write;
use std::sync::Arc;

use anyhow::Result;
use blobstore::Blobstore;
use fbinit::FacebookInit;
use filestore::hash_bytes;
use filestore::Sha1IncrementalHasher;
use git_hash::ObjectId;
use git_object::Tag;
use git_object::WriteTo;

use crate::CoreContext;
use crate::Repo;
use crate::RepoContext;

async fn init_repo(ctx: &CoreContext) -> Result<RepoContext> {
    let blob_repo = test_repo_factory::build_empty(ctx.fb)?;
    let repo = Repo::new_test(ctx.clone(), blob_repo).await?;
    let repo_context = RepoContext::new_test(ctx.clone(), Arc::new(repo)).await?;
    Ok(repo_context)
}

#[fbinit::test]
/// Validate the basic git upload object functionality works.
async fn basic_upload_git_object(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo_ctx = init_repo(&ctx).await?;
    let tag = Tag {
        target: ObjectId::empty_tree(git_hash::Kind::Sha1),
        target_kind: git_object::Kind::Tree,
        name: "TreeTag".into(),
        tagger: None,
        message: "Tag pointing to a tree".into(),
        pgp_signature: None,
    };
    let mut bytes = tag.loose_header().into_vec();
    tag.write_to(bytes.by_ref())?;

    let bytes_to_hash = bytes::Bytes::from(bytes.clone());
    let sha1_hash = hash_bytes(Sha1IncrementalHasher::new(), &bytes_to_hash);
    repo_ctx
        .upload_git_object(git_hash::oid::try_from_bytes(sha1_hash.as_ref())?, bytes)
        .await
}

#[fbinit::test]
/// Validate that we get an error while trying to upload a git blob through this method.
async fn blob_upload_git_object(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo_ctx = init_repo(&ctx).await?;
    let blob = git_object::Blob {
        data: "Some file content".as_bytes().to_vec(),
    };
    let mut bytes = blob.loose_header().into_vec();
    blob.write_to(bytes.by_ref())?;
    let bytes_to_hash = bytes::Bytes::from(bytes.clone());
    let sha1_hash = hash_bytes(Sha1IncrementalHasher::new(), &bytes_to_hash);
    let output = repo_ctx
        .upload_git_object(git_hash::oid::try_from_bytes(sha1_hash.as_ref())?, bytes)
        .await;
    assert!(output.is_err());
    assert!(
        output
            .unwrap_err()
            .to_string()
            .contains("upload_git_object cannot be used to upload raw file content.")
    );
    Ok(())
}

#[fbinit::test]
/// Validate that we get an error while trying to upload invalid git bytes with this method.
async fn invalid_bytes_upload_git_object(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo_ctx = init_repo(&ctx).await?;
    let tag = Tag {
        target: ObjectId::empty_tree(git_hash::Kind::Sha1),
        target_kind: git_object::Kind::Tree,
        name: "TreeTag".into(),
        tagger: None,
        message: "Tag pointing to a tree".into(),
        pgp_signature: None,
    };
    let mut bytes = Vec::new();
    tag.write_to(bytes.by_ref())?;

    let bytes_to_hash = bytes::Bytes::from(bytes.clone());
    let sha1_hash = hash_bytes(Sha1IncrementalHasher::new(), &bytes_to_hash);
    let output = repo_ctx
        .upload_git_object(git_hash::oid::try_from_bytes(sha1_hash.as_ref())?, bytes)
        .await;
    assert!(output.is_err());
    assert!(
        output
            .unwrap_err()
            .to_string()
            .contains("Invalid git object data for")
    );
    Ok(())
}

#[fbinit::test]
/// Validate that we get an error while trying to upload a git object with incorrect hash.
async fn invalid_hash_upload_git_object(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo_ctx = init_repo(&ctx).await?;
    let tag = Tag {
        target: ObjectId::empty_tree(git_hash::Kind::Sha1),
        target_kind: git_object::Kind::Tree,
        name: "TreeTag".into(),
        tagger: None,
        message: "Tag pointing to a tree".into(),
        pgp_signature: None,
    };
    let mut bytes = tag.loose_header().into_vec();
    tag.write_to(bytes.by_ref())?;

    let output = repo_ctx
        .upload_git_object(&ObjectId::empty_tree(git_hash::Kind::Sha1), bytes)
        .await;
    assert!(output.is_err());
    assert!(
        output
            .unwrap_err()
            .to_string()
            .contains("does not match hash of bytes")
    );
    Ok(())
}

#[fbinit::test]
/// Validate that the git object stored in the blobstore is stored under the right key.
async fn blobstore_check_upload_git_object(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo_ctx = init_repo(&ctx).await?;
    let tag = Tag {
        target: ObjectId::empty_tree(git_hash::Kind::Sha1),
        target_kind: git_object::Kind::Tree,
        name: "TreeTag".into(),
        tagger: None,
        message: "Tag pointing to a tree".into(),
        pgp_signature: None,
    };
    let mut bytes = tag.loose_header().into_vec();
    tag.write_to(bytes.by_ref())?;

    let bytes_to_hash = bytes::Bytes::from(bytes.clone());
    let sha1_hash = hash_bytes(Sha1IncrementalHasher::new(), &bytes_to_hash);
    let blobstore_key = format!("git_object_{}", sha1_hash.to_hex());
    repo_ctx
        .upload_git_object(git_hash::oid::try_from_bytes(sha1_hash.as_ref())?, bytes)
        .await?;
    let output = repo_ctx.repo_blobstore().get(&ctx, &blobstore_key).await?;
    assert!(output.is_some());
    Ok(())
}
