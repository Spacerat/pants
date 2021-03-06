# Copyright 2018 Pants project contributors (see CONTRIBUTORS.md).
# Licensed under the Apache License, Version 2.0 (see LICENSE).

import logging
from typing import Iterable, Optional, cast

from pants.base.build_environment import get_buildroot, get_scm
from pants.base.cmd_line_spec_parser import CmdLineSpecParser
from pants.base.specs import (
    AddressSpec,
    AddressSpecs,
    FilesystemLiteralSpec,
    FilesystemSpec,
    FilesystemSpecs,
    SingleAddress,
    Specs,
)
from pants.engine.internals.scheduler import SchedulerSession
from pants.engine.internals.selectors import Params
from pants.option.options import Options
from pants.option.options_bootstrapper import OptionsBootstrapper
from pants.scm.subsystems.changed import ChangedAddresses, ChangedOptions, ChangedRequest
from pants.util.ordered_set import OrderedSet

logger = logging.getLogger(__name__)


class InvalidSpecConstraint(Exception):
    """Raised when invalid constraints are given via specs and arguments like --changed*."""


class SpecsCalculator:
    """Determines the specs for a given Pants run."""

    @classmethod
    def parse_specs(cls, raw_specs: Iterable[str], *, build_root: Optional[str] = None) -> Specs:
        """Parse raw string specs into a Specs object."""
        build_root = build_root or get_buildroot()
        spec_parser = CmdLineSpecParser(build_root)

        address_specs: OrderedSet[AddressSpec] = OrderedSet()
        filesystem_specs: OrderedSet[FilesystemSpec] = OrderedSet()
        for spec_str in raw_specs:
            parsed_spec = spec_parser.parse_spec(spec_str)
            if isinstance(parsed_spec, AddressSpec):
                address_specs.add(parsed_spec)
            else:
                filesystem_specs.add(parsed_spec)

        return Specs(
            AddressSpecs(address_specs, filter_by_global_options=True),
            FilesystemSpecs(filesystem_specs),
        )

    @classmethod
    def create(
        cls,
        options_bootstrapper: OptionsBootstrapper,
        options: Options,
        session: SchedulerSession,
        build_root: Optional[str] = None,
    ) -> Specs:
        specs = cls.parse_specs(raw_specs=options.specs, build_root=build_root)
        changed_options = ChangedOptions.from_options(options.for_scope("changed"))

        logger.debug("specs are: %s", specs)
        logger.debug("changed_options are: %s", changed_options)

        if specs.provided and changed_options.provided:
            changed_name = "--changed-since" if changed_options.since else "--changed-diffspec"
            if specs.filesystem_specs and specs.address_specs:
                specs_description = "target and file arguments"
            elif specs.filesystem_specs:
                specs_description = "file arguments"
            else:
                specs_description = "target arguments"
            raise InvalidSpecConstraint(
                f"You used `{changed_name}` at the same time as using {specs_description}. Please "
                "use only one."
            )

        if not changed_options.provided:
            return specs

        scm = get_scm()
        if not scm:
            raise InvalidSpecConstraint(
                "The `--changed-*` options are not available without a recognized SCM (usually "
                "Git)."
            )
        changed_request = ChangedRequest(
            sources=tuple(changed_options.changed_files(scm=scm)),
            dependees=changed_options.dependees,
        )
        (changed_addresses,) = session.product_request(
            ChangedAddresses, [Params(changed_request, options_bootstrapper)]
        )
        logger.debug("changed addresses: %s", changed_addresses)

        address_specs = []
        filesystem_specs = []
        for address in cast(ChangedAddresses, changed_addresses):
            if not address.is_base_target:
                # TODO: Should adjust Specs parsing to support parsing the disambiguated file
                # Address, which would bypass-rediscovering owners.
                filesystem_specs.append(FilesystemLiteralSpec(address.filename))
            else:
                address_specs.append(SingleAddress(address.spec_path, address.target_name))

        return Specs(
            AddressSpecs(address_specs, filter_by_global_options=True),
            FilesystemSpecs(filesystem_specs),
        )
