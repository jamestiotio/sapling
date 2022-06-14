# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"
  $ ENABLED_DERIVED_DATA='["git_trees", "filenodes", "hgchangesets"]' setup_common_config
  $ GIT_REPO="${TESTTMP}/repo-git"
  $ HG_REPO="${TESTTMP}/repo-hg"

# Setup git repository
  $ mkdir "$GIT_REPO"
  $ cd "$GIT_REPO"
  $ git init -q
  $ echo "this is file1" > file1
  $ git add file1
  $ git commit -am "Add file1"
  [master (root-commit) 8ce3eae] Add file1
   1 file changed, 1 insertion(+)
   create mode 100644 file1
  $ git log
  commit 8ce3eae44760b500bf3f2c3922a95dcd3c908e9e
  Author: mononoke <mononoke@mononoke>
  Date:   Sat Jan 1 00:00:00 2000 +0000
  
      Add file1

  $ cd "$TESTTMP"
  $ git clone repo-git repo-git-clone
  Cloning into 'repo-git-clone'...
  done.
  $ cd "$TESTTMP/repo-git"
  $ git checkout --orphan another_committer
  Switched to a new branch 'another_committer'
  $ echo "this is file1" > file1
  $ git add file1
  $ export GIT_COMMITTER_NAME="second_committer"
  $ export GIT_COMMITTER_EMAIL="second_committer@fb.com"
  $ export GIT_COMMITTER_DATE="1000000000"
  $ git_set_only_author commit -am "Add file1"
  [another_committer (root-commit) 69a2653] Add file1
   Author: mononoke <mononoke@mononoke>
   1 file changed, 1 insertion(+)
   create mode 100644 file1
  $ git log --pretty=fuller
  commit 69a265312a2c29cdf5667ff401d895a66e6ac02a
  Author:     mononoke <mononoke@mononoke>
  AuthorDate: Sat Jan 1 00:00:00 2000 +0000
  Commit:     second_committer <second_committer@fb.com>
  CommitDate: Sun Sep 9 01:46:40 2001 +0000
  
      Add file1

# Import it into Mononoke
  $ cd "$TESTTMP"
  $ gitimport "$GIT_REPO" --derive-hg full-repo
  * using repo "repo" repoid RepositoryId(0) (glob)
  * GitRepo:$TESTTMP/repo-git commit 1 of 2 - Oid:8ce3eae4 => Bid:d4229e98 (glob)
  * GitRepo:$TESTTMP/repo-git commit 2 of 2 - Oid:69a26531 => Bid:6f60c3c6 (glob)
  * Hg: Sha1(8ce3eae44760b500bf3f2c3922a95dcd3c908e9e): HgManifestId(HgNodeHash(Sha1(009adbc8d457927d2e1883c08b0692bc45089839))) (glob)
  * Hg: Sha1(69a265312a2c29cdf5667ff401d895a66e6ac02a): HgManifestId(HgNodeHash(Sha1(009adbc8d457927d2e1883c08b0692bc45089839))) (glob)
  * Ref: "refs/heads/another_committer": Some(ChangesetId(Blake2(6f60c3c647ab1eab6ce671f3a20e7a9d42aa424dffa927bfc69cd17ff0d56dd1))) (glob)
  * Ref: "refs/heads/master": Some(ChangesetId(Blake2(d4229e9850e9244c3a986a62590ffada646e7200593bc26e4cc8c9aa10730a26))) (glob)

# Check hg hash before overwriting
  $ mononoke_admin convert --from bonsai --to hg 6f60c3c647ab1eab6ce671f3a20e7a9d42aa424dffa927bfc69cd17ff0d56dd1
  * using repo "repo" repoid RepositoryId(0) (glob)
  b7e825fd5693ac646d85a5c61fa1fae30b980abe

# Now rederive HG_SET_COMMITTER_EXTRA=true. This changes hg hash, so let's run it with --rederive and make sure
# hg hash was overwritten.
  $ HG_SET_COMMITTER_EXTRA=true ENABLED_DERIVED_DATA='["git_trees", "filenodes", "hgchangesets"]' setup_common_config
  $ mononoke_admin derived-data derive hgchangesets --changeset 6f60c3c647ab1eab6ce671f3a20e7a9d42aa424dffa927bfc69cd17ff0d56dd1 --rederive
  * using repo "repo" repoid RepositoryId(0) (glob)
  * changeset resolved as: ChangesetId(Blake2(6f60c3c647ab1eab6ce671f3a20e7a9d42aa424dffa927bfc69cd17ff0d56dd1)) (glob)
  * about to rederive 1 commits (glob)
  * deriving 6f60c3c647ab1eab6ce671f3a20e7a9d42aa424dffa927bfc69cd17ff0d56dd1 (glob)
  * derived 6f60c3c647ab1eab6ce671f3a20e7a9d42aa424dffa927bfc69cd17ff0d56dd1 in *, Ok("MappedHgChangesetId(HgChangesetId(HgNodeHash(Sha1(ccb82015a1dc22d5b13a1b83855bbe39659692f3))))") (glob)

# Check hg hash after overwriting
  $ mononoke_admin convert --from bonsai --to hg 6f60c3c647ab1eab6ce671f3a20e7a9d42aa424dffa927bfc69cd17ff0d56dd1
  * using repo "repo" repoid RepositoryId(0) (glob)
  ccb82015a1dc22d5b13a1b83855bbe39659692f3
