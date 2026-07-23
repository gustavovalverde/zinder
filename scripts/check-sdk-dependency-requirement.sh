#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 4 ]]; then
  echo >&2 "usage: check-sdk-dependency-requirement.sh WORKSPACE_VERSION PACKAGE DEPENDENCY REQUIREMENT"
  exit 2
fi

workspace_version="$1"
package_name="$2"
dependency_name="$3"
actual_requirement="$4"
expected_requirement="^${workspace_version}"

[[ "$actual_requirement" == "$expected_requirement" ]] || {
  echo >&2 \
    "$package_name -> $dependency_name requires $actual_requirement, expected $expected_requirement"
  exit 1
}
