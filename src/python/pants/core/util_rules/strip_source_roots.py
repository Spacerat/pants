# Copyright 2019 Pants project contributors (see CONTRIBUTORS.md).
# Licensed under the Apache License, Version 2.0 (see LICENSE).

import itertools
from dataclasses import dataclass

from pants.core.util_rules.determine_source_files import SourceFiles
from pants.engine.fs import Digest, DigestSubset, MergeDigests, PathGlobs, RemovePrefix, Snapshot
from pants.engine.rules import Get, MultiGet, collect_rules, rule
from pants.source.source_root import SourceRootsRequest, SourceRootsResult


@dataclass(frozen=True)
class StrippedSourceFiles:
    """Wrapper for a snapshot of files whose source roots have been stripped."""

    snapshot: Snapshot


@rule
async def strip_source_roots(source_files: SourceFiles) -> StrippedSourceFiles:
    """Removes source roots from a snapshot.

    E.g. `src/python/pants/util/strutil.py` -> `pants/util/strutil.py`.
    """
    if not source_files.snapshot.files:
        return StrippedSourceFiles(source_files.snapshot)

    if source_files.unrooted_files:
        rooted_files = set(source_files.snapshot.files) - set(source_files.unrooted_files)
        rooted_files_snapshot = await Get(
            Snapshot, DigestSubset(source_files.snapshot.digest, PathGlobs(rooted_files))
        )
    else:
        rooted_files_snapshot = source_files.snapshot

    source_roots_result = await Get(
        SourceRootsResult,
        SourceRootsRequest,
        SourceRootsRequest.for_files(rooted_files_snapshot.files),
    )

    file_to_source_root = {
        str(file): root for file, root in source_roots_result.path_to_root.items()
    }
    files_grouped_by_source_root = {
        source_root.path: tuple(str(f) for f in files)
        for source_root, files in itertools.groupby(
            file_to_source_root.keys(), key=file_to_source_root.__getitem__
        )
    }

    if len(files_grouped_by_source_root) == 1:
        source_root = next(iter(files_grouped_by_source_root.keys()))
        if source_root == ".":
            resulting_snapshot = rooted_files_snapshot
        else:
            resulting_snapshot = await Get(
                Snapshot, RemovePrefix(rooted_files_snapshot.digest, source_root)
            )
    else:
        digest_subsets = await MultiGet(
            Get(Digest, DigestSubset(rooted_files_snapshot.digest, PathGlobs(files)))
            for files in files_grouped_by_source_root.values()
        )
        resulting_digests = await MultiGet(
            Get(Digest, RemovePrefix(digest, source_root))
            for digest, source_root in zip(digest_subsets, files_grouped_by_source_root.keys())
        )
        resulting_snapshot = await Get(Snapshot, MergeDigests(resulting_digests))

    # Add the unrooted files back in.
    if source_files.unrooted_files:
        unrooted_files_digest = await Get(
            Digest,
            DigestSubset(source_files.snapshot.digest, PathGlobs(source_files.unrooted_files)),
        )
        resulting_snapshot = await Get(
            Snapshot, MergeDigests((resulting_snapshot.digest, unrooted_files_digest))
        )

    return StrippedSourceFiles(resulting_snapshot)


def rules():
    return collect_rules()
