# Copyright 2022 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""
Parses the intermediate JSON provided and generates a new JSON containing
metadata about FHCP tests.
"""
import argparse
import json
import sys
from typing import Any


def validate(data: dict[str, Any]) -> bool:
    tests = data["tests"]
    test_types = data["driver_test_types"]
    categories = data["device_category_types"]
    for test in tests:
        url = test["url"]
        if not test["test_types"]:
            raise ValueError(
                f"The test {url} must specify at least one category."
            )
        for item in test["test_types"]:
            if item not in test_types:
                raise ValueError(
                    f"The test {url} specifies an invalid category '{item}'."
                )
        if not test["device_categories"]:
            raise ValueError(f"The test {url} must specify at least one type.")
        for item in test["device_categories"]:
            item_type = item["category"]
            if item_type not in categories:
                raise ValueError(
                    f"The test {url} specifies an invalid type '{item_type}'."
                )
            item_sub_type = item["subcategory"]
            if item_sub_type and item_sub_type not in categories[item_type]:
                raise ValueError(
                    f"The test {url} specifies an invalid sub-type '{item_sub_type}'."
                )
        if "is_automated" not in test:
            raise ValueError(
                f"The test {url} must specify an 'is_automated' value."
            )
    return True


def check_required_fhcp_fields(d: dict[str, Any]) -> None:
    # "test_types" key must exist and have a non-empty value.
    if "test_types" not in d or not d["test_types"]:
        raise ValueError(
            "The 'test_types' field must have at least one type defined. Missing from:",
            d,
        )
    if "device_categories" not in d or not d["device_categories"]:
        raise ValueError(
            "The 'device_categories' field must have at least one category defined. Missing from:",
            d,
        )
    for category in d["device_categories"]:
        if "category" not in category:
            raise ValueError(
                "Missing 'category' in category definition:", category
            )
        if not category["category"]:
            raise ValueError("Category field must have a category value.")
        # Subcategory can be empty, so there is nothing to enforce.
    if "environments" not in d or not d["environments"]:
        raise ValueError(
            "The 'environments' field must have at least one environment defined. Missing from:",
            d,
        )
    for environment in d["environments"]:
        if "tags" not in environment:
            raise ValueError(
                "Missing 'tags' in environment definition:", environment
            )
        if (
            "fhcp-automated" not in environment["tags"]
            and "fhcp-manual" not in environment["tags"]
        ):
            raise ValueError(
                "The 'tags' field must have at least one tag of either 'fhcp-automated' or 'fhcp-manual'. Missing from:",
                environment["tags"],
            )


def convert_to_final_dict(
    appendix: dict[str, Any], data: list[dict[str, Any]]
) -> dict[str, Any]:
    test_entries = {}
    fhcp_entries = {}

    # The `data` is a mix of two different sources of JSON.
    # Type A is generated by `fuchsia_test_package` under the "tests"
    # metadata field.
    # Type B is generated by `fhcp_test_package` under the "fhcp"
    # metadata field.
    for d in data:
        if "test" in d and "build_rule" in d["test"]:
            # Found a "Type A" as mentioned above.
            test_entries[d["test"]["package_label"]] = d
        elif "test_types" in d and "device_categories" in d:
            # Found a "Type B" as described above.
            check_required_fhcp_fields(d)
            fhcp_entries[d["id"]] = d
        else:
            raise ValueError("This is not a valid entry:", d)
    tests = []
    for entry in fhcp_entries:
        if entry not in test_entries:
            raise ValueError(f"Did not find '{entry}' in the tests.")
        test_metadata = test_entries[entry]
        fhcp_metadata = fhcp_entries[entry]
        is_automated = (
            len(fhcp_metadata["environments"]) > 0
            and fhcp_metadata["environments"][0]
            and "fhcp-automated" in fhcp_metadata["environments"][0]["tags"]
        )
        tests.append(
            {
                "url": test_metadata["test"]["package_url"],
                "test_types": fhcp_metadata["test_types"],
                "device_categories": fhcp_metadata["device_categories"],
                "is_automated": is_automated,
            }
        )
    appendix["tests"] = tests
    return appendix


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--appendix_json", help="Path to the appendix JSON", required=True
    )
    parser.add_argument(
        "--intermediate_json", help="Path to intermediate JSON", required=True
    )
    parser.add_argument(
        "--output_json", help="Path that we will output to.", required=True
    )
    args = parser.parse_args()

    appendix = {}
    with open(args.appendix_json, "r") as f:
        appendix = json.load(f)
    intermediate = []
    with open(args.intermediate_json, "r") as f:
        intermediate = json.load(f)

    final_dict = convert_to_final_dict(appendix, intermediate)
    validate(final_dict)

    with open(args.output_json, "w") as output:
        output.write(str(json.dumps(final_dict)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
