# Copyright 2020 Pants project contributors (see CONTRIBUTORS.md).
# Licensed under the Apache License, Version 2.0 (see LICENSE).

from dataclasses import dataclass
from typing import Tuple

from pants.backend.python.lint.docformatter.subsystem import Docformatter
from pants.backend.python.lint.python_fmt import PythonFmtRequest
from pants.backend.python.rules import pex
from pants.backend.python.rules.pex import (
    Pex,
    PexInterpreterConstraints,
    PexProcess,
    PexRequest,
    PexRequirements,
)
from pants.backend.python.target_types import PythonSources
from pants.core.goals.fmt import FmtResult
from pants.core.goals.lint import LintRequest, LintResult, LintResults
from pants.core.util_rules import determine_source_files, strip_source_roots
from pants.core.util_rules.determine_source_files import SourceFiles, SourceFilesRequest
from pants.engine.fs import Digest, MergeDigests
from pants.engine.process import FallibleProcessResult, Process, ProcessResult
from pants.engine.rules import Get, MultiGet, collect_rules, rule
from pants.engine.target import FieldSet
from pants.engine.unions import UnionRule
from pants.util.strutil import pluralize


@dataclass(frozen=True)
class DocformatterFieldSet(FieldSet):
    required_fields = (PythonSources,)

    sources: PythonSources


class DocformatterRequest(PythonFmtRequest, LintRequest):
    field_set_type = DocformatterFieldSet


@dataclass(frozen=True)
class SetupRequest:
    request: DocformatterRequest
    check_only: bool


@dataclass(frozen=True)
class Setup:
    process: Process
    original_digest: Digest


def generate_args(
    *, source_files: SourceFiles, docformatter: Docformatter, check_only: bool,
) -> Tuple[str, ...]:
    return ("--check" if check_only else "--in-place", *docformatter.args, *source_files.files)


@rule
async def setup(setup_request: SetupRequest, docformatter: Docformatter) -> Setup:
    requirements_pex_request = Get(
        Pex,
        PexRequest(
            output_filename="docformatter.pex",
            requirements=PexRequirements(docformatter.all_requirements),
            interpreter_constraints=PexInterpreterConstraints(docformatter.interpreter_constraints),
            entry_point=docformatter.entry_point,
        ),
    )

    source_files_request = Get(
        SourceFiles,
        SourceFilesRequest(field_set.sources for field_set in setup_request.request.field_sets),
    )

    source_files, requirements_pex = await MultiGet(source_files_request, requirements_pex_request)

    source_files_snapshot = (
        source_files.snapshot
        if setup_request.request.prior_formatter_result is None
        else setup_request.request.prior_formatter_result
    )

    input_digest = await Get(
        Digest, MergeDigests((source_files_snapshot.digest, requirements_pex.digest))
    )

    address_references = ", ".join(
        sorted(field_set.address.spec for field_set in setup_request.request.field_sets)
    )

    process = await Get(
        Process,
        PexProcess(
            requirements_pex,
            argv=generate_args(
                source_files=source_files,
                docformatter=docformatter,
                check_only=setup_request.check_only,
            ),
            input_digest=input_digest,
            output_files=source_files_snapshot.files,
            description=(
                f"Run Docformatter on {pluralize(len(setup_request.request.field_sets), 'target')}: "
                f"{address_references}."
            ),
        ),
    )
    return Setup(process, original_digest=source_files_snapshot.digest)


@rule(desc="Format Python docstrings with docformatter")
async def docformatter_fmt(request: DocformatterRequest, docformatter: Docformatter) -> FmtResult:
    if docformatter.skip:
        return FmtResult.noop()
    setup = await Get(Setup, SetupRequest(request, check_only=False))
    result = await Get(ProcessResult, Process, setup.process)
    return FmtResult.from_process_result(
        result, original_digest=setup.original_digest, formatter_name="Docformatter"
    )


@rule(desc="Lint Python docstrings with docformatter")
async def docformatter_lint(
    request: DocformatterRequest, docformatter: Docformatter
) -> LintResults:
    if docformatter.skip:
        return LintResults()
    setup = await Get(Setup, SetupRequest(request, check_only=True))
    result = await Get(FallibleProcessResult, Process, setup.process)
    return LintResults(
        [LintResult.from_fallible_process_result(result, linter_name="Docformatter")]
    )


def rules():
    return [
        *collect_rules(),
        UnionRule(PythonFmtRequest, DocformatterRequest),
        UnionRule(LintRequest, DocformatterRequest),
        *determine_source_files.rules(),
        *pex.rules(),
        *strip_source_roots.rules(),
    ]
