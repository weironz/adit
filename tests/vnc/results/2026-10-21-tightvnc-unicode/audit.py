"""Offline audit of preserved real TightVNC results, including known failure.

An exit code of zero means this archive is internally consistent. It does not
mean that TightVNC passed the non-BMP case or that live cleanup was rerun.
"""

import hashlib
import json
from pathlib import Path
import re


ROOT = Path(__file__).resolve().parent
CASES = {
    "ascii": ["U+0061", "U+0062", "U+0063"],
    "bmp-cjk": ["U+4E2D", "U+6587"],
    "nonbmp-emoji": ["U+1F60A"],
    "combining": ["U+0065", "U+0301"],
}


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def read(relative):
    return json.loads((ROOT / relative).read_text(encoding="utf-8-sig"))


def scalars(text):
    return [f"U+{ord(ch):04X}" for ch in text]


def check_case(directory, prefix, case):
    stem = f"{directory}/{prefix}{case}"
    result = read(f"{stem}-result.json")
    actual = read(f"{stem}-actual.json")
    expected = CASES[case]
    observed = ["U+F60A"] if case == "nonbmp-emoji" else expected
    exact = case != "nonbmp-emoji"
    require(result["case"] == case, f"{stem}: case identity")
    require(result["expected_scalars"] == expected, f"{stem}: expected scalars")
    require(scalars(result["expected"]) == expected, f"{stem}: expected text")
    require(result["after"] == actual, f"{stem}: raw TextBox model differs")
    require(actual["scalars"] == observed, f"{stem}: observed scalars")
    require(scalars(actual["text"]) == observed, f"{stem}: observed text")
    require(result["outcome"] == ("passed" if exact else "failed"), f"{stem}: outcome")
    require(result["clipboard_send_used"] is False, f"{stem}: clipboard used")
    require(result["driver_exit"] == 0, f"{stem}: driver exit")
    require(result["checks"]["exact_scalars"] is exact, f"{stem}: scalar check")
    require(set(result["checks"]) == {
        "driver_ready", "driver_exit_zero", "same_target", "target_focused",
        "all_input_focused", "actual_input", "exact_scalars",
    }, f"{stem}: unexpected check schema")
    for name, value in result["checks"].items():
        if name != "exact_scalars":
            require(value is True, f"{stem}: {name}")
    for stage in ("before", "after"):
        model = result[stage]
        native = result[f"native_{stage}"]
        require(model["focused"] is True, f"{stem}: {stage} focus")
        require(native["gui_ok"] is True, f"{stem}: {stage} native focus query")
        require(native["foreground_pid"] == model["pid"], f"{stem}: foreground PID")
        require(native["focus_hwnd"] == model["box_hwnd"], f"{stem}: focused TextBox")
        require(model["unfocused_input"] == model["focus_losses"] == 0, f"{stem}: lost focus")
    before = result["before"]
    require(before["pid"] == actual["pid"] and before["box_hwnd"] == actual["box_hwnd"], f"{stem}: target changed")
    require(before["text"] == "" and before["scalars"] == [], f"{stem}: target not empty")
    require(before["press"] == before["keypress"] == before["text_changes"] == 0, f"{stem}: input baseline")
    require(actual["press"] > 0 and actual["keypress"] > 0 and actual["text_changes"] > 0, f"{stem}: no actual input")
    stdout = (ROOT / f"{stem}-driver.stdout.txt").read_text(encoding="utf-8-sig")
    require(re.fullmatch(r"ready_ms=\d+\s*", stdout) is not None, f"{stem}: readiness output")
    require((ROOT / f"{stem}-driver.stderr.txt").read_bytes() == b"", f"{stem}: driver stderr")
    return result


def main():
    manifest = read("provenance.json")
    require(manifest["schema"] == 1, "Manifest schema")
    paths = [entry["path"] for entry in manifest["files"]]
    require(len(paths) == len(set(paths)) == 41, "Allowlisted evidence/source count")
    for entry in manifest["files"]:
        relative = Path(entry["path"])
        require(not relative.is_absolute() and ".." not in relative.parts, "Unsafe manifest path")
        data = (ROOT / relative).read_bytes()
        require(len(data) == entry["bytes"], f"{relative}: byte length")
        require(hashlib.sha256(data).hexdigest() == entry["sha256"], f"{relative}: SHA-256")
    first = read("first/tight-unicode1-preflight.json")
    first_owned = read("first/tight-unicode1-ownership.json")
    second = read("second/tight-unicode2-result.json")
    preflight = read("second/tight-unicode2-preflight.json")
    journal = read("second/tight-unicode2-fixture-ownership.json")
    require(second["outcome"] == "failed", "Known non-BMP failure must remain failed")
    require(second["cleanup_verified"] is True and second["original_state_unchanged"] is True, "Cleanup summary")
    require(second["owned_registry_removed"] is True and second["cleanup_errors"] == [], "Registry/cleanup errors")
    require(second["remaining_fixture_tcp"] == second["remaining_fixture_udp"] == 0, "Remaining fixture listeners")
    require(second["before"] == second["after"] == preflight["before"], "Original state baseline differs")
    require(second["clipboard_send_used"] is False, "Root clipboard flag")
    require(second["server_signature"]["status"] == "Valid", "Server signature")
    require(second["server_version"] == "2, 8, 90, 0", "Server version")
    require(second["server_sha256"] == manifest["server_sha256"], "Server hash")
    require(second["driver_sha256"] == manifest["driver_sha256"], "Driver hash")
    require(second["registry_owner_token"] == journal["registry_owner_token"], "Second ownership token")
    require(journal["registry_owned"] is True, "Journal must preserve pre-cleanup ownership")
    require(second["server_identity"] == journal["server"], "Server identity")
    require(second["owned_processes"] == journal["owned_processes"], "Process journal differs")
    require(second["fixture_udp"] == [], "Fixture UDP")
    require(second["fixture_tcp"] == [{"LocalAddress": "127.0.0.1", "LocalPort": 15911,
                                       "OwningProcess": journal["server"]["id"]}], "Fixture loopback listener")
    original_ids = {p["ProcessId"] for p in second["before"]["processes"]}
    require(original_ids == {3420, 11124}, "Original TightVNC process identities")
    require(original_ids.isdisjoint(p["id"] for p in journal["owned_processes"]), "Original process in owned list")
    for identity in journal["owned_processes"]:
        require(identity["start_ticks"] > 10**17, "Truncated process start ticks")
        require(identity["path"].casefold().startswith(journal["payload"].casefold() + "\\"), "Non-owned payload process")
    for case in CASES:
        check_case("first/tight-unicode1-first", "", case)
        result = check_case("second", "tight-unicode2-", case)
        require(result == next(c for c in second["cases"] if c["case"] == case), f"{case}: root case differs")
    require(len(second["cases"]) == 4, "Unexpected second-run case count")
    require(len({c["before"]["pid"] for c in second["cases"]}) == 4, "Cases must use independent target processes")
    failed = read("recovery/tight-recovery1-failed-utf8.json")
    require(failed["exited"] is True and failed["exit_code"] == 1, "Failed recovery exit")
    require(failed["recovery"]["outcome"] == "failed", "Failed recovery changed")
    require(failed["recovery"]["process_actions"] == [], "Failed UTF-8 recovery performed process actions")
    require(failed["recovery"]["registry_removed"] is False, "Failed recovery registry claim")
    require(bool(failed["recovery"].get("error")), "Failed recovery error missing")
    wrapped = read("recovery/tight-recovery2.json")
    recovered = wrapped["recovery"]
    require(wrapped["exited"] is True and wrapped["exit_code"] == 0, "Independent recovery exit")
    require(recovered["outcome"] == "passed", "Independent recovery outcome")
    require(recovered["registry_removed"] is True and recovered["original_state_unchanged"] is True, "Independent cleanup")
    require(recovered["unknown_processes"] == [], "Unknown owned-payload process")
    require(recovered["fixture_tcp"] == recovered["fixture_udp"] == [], "Independent recovery listeners")
    require(recovered["after"] == first["before"], "First-run baseline changed")
    require(recovered["registry_owner_token"] == first_owned["registry_owner_token"], "First ownership token")
    require(recovered["recorded_identities"] == first_owned["owned_processes"], "First recovery identities")
    require(len(recovered["process_actions"]) == len(recovered["recorded_identities"]) == 10, "Recovery action coverage")
    require(all(a["observed"] == "exited" and a["terminated"] is False for a in recovered["process_actions"]), "Recovery had to terminate a process")
    print("Archive audit passed: hashes/focus/native TextBox consumption/cleanup verified from preserved evidence.")
    print("Second run: 3 passed; 1 KNOWN COMPATIBILITY FAILURE (U+1F60A -> U+F60A); root remains failed.")
    print("First-run summary infrastructure failure and failed UTF-8 recovery retained; independent recovery passed.")


if __name__ == "__main__":
    main()
