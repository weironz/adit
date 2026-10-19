"""Audit captured Windows input effects, keeping incomplete experiments open."""
import argparse
import hashlib
import json
from datetime import datetime
from pathlib import Path


def document(root, name):
    return json.loads((root / name).read_text(encoding="utf-8-sig"))


def rows(root, name):
    return [json.loads(line) for line in
            (root / name).read_text(encoding="utf-8-sig").splitlines() if line]


def pairs(*symbols):
    return [(symbol, down) for symbol in symbols for down in (True, False)]


def transition(root, before, after, expected):
    previous, current = rows(root, before), rows(root, after)
    assert current[:len(previous)] == previous, f"not an append-only capture: {after}"
    keys = [(event["keysym"], event["down"])
            for event in current[len(previous):] if event["event"] == "key"]
    assert keys == expected, f"unexpected native input in {after}: {keys!r}"
    return len(keys)


def audit(root):
    manifest = document(root, "sha256.json")["files"]
    actual_files = {str(path.relative_to(root)).replace("\\", "/")
                    for path in root.rglob("*") if path.is_file()
                    and path.name != "sha256.json"}
    assert set(manifest) == actual_files, "artifact inventory changed"
    for name, expected in manifest.items():
        path = (root / name).resolve()
        assert path.is_relative_to(root.resolve()), "artifact path escapes archive"
        assert hashlib.sha256(path.read_bytes()).hexdigest() == expected, name
    provenance = document(root, "provenance.json")
    assert hashlib.sha256((root / "native-runtime.patch").read_bytes()).hexdigest() == (
        provenance["native_runtime_patch_sha256"])
    assert hashlib.sha256((root / "observer-source.py").read_bytes()).hexdigest() == (
        provenance["observer_source_sha256"])
    ready = document(root, "final-ready.json")
    focus = document(root, "final-focus.json")
    assert ready["app"] == focus["owner"] == focus["expected"]
    assert focus["ok"] and ready["observer"] is None and ready["session"] == 1
    assert ready["source_binary_sha256"].lower() == (
        "2dff3d9b74f6e8777c358fd8d67a522bfc061c14b9e747c3b6b24848c5357121")

    counts = {}
    # This asynchronous read completed after the physical German baseline,
    # before the US character sample. Its request name is not a time boundary.
    baseline_keys = [(event["keysym"], event["down"])
                     for event in rows(root, "final-before-layout.jsonl")
                     if event["event"] == "key"]
    assert baseline_keys[-4:] == pairs(ord("z"), ord("y"))
    counts["german_baseline"] = 4
    counts["german_to_us"] = transition(
        root, "final-before-layout.jsonl", "final-german-us.jsonl",
        pairs(ord("y"), ord("z")))
    german = pairs(ord("z"), ord("y")) + [
        (0xffe2, True), (ord("Y"), True), (ord("Y"), False), (0xffe2, False),
        (0xffe4, True), (ord("a"), True), (ord("a"), False), (0xffe4, False),
        (0xfe03, True), (ord("@"), True), (ord("@"), False), (0xfe03, False),
        (0xfe03, True), (0x010020ac, True), (0x010020ac, False), (0xfe03, False),
    ] + pairs(ord("é"))
    counts["us_to_german_altgr_dead_key"] = transition(
        root, "final-german-us.jsonl", "final-us-german.jsonl", german)
    counts["pinyin_preedit"] = transition(
        root, "final-us-german.jsonl", "final-pinyin-preedit.jsonl", [])
    counts["pinyin_commit"] = transition(
        root, "final-pinyin-preedit.jsonl", "final-pinyin-commit.jsonl",
        pairs(0x01004e2d, 0x01006587))
    counts["emoji_picker_commit"] = transition(
        root, "final-pinyin-commit.jsonl", "final-emoji.jsonl", pairs(0x0101f60a))
    assert not any(event.get("keysym") in (0xffeb, 0xffec)
                   for event in rows(root, "final-emoji.jsonl"))

    # Registration success and a green toolbar were insufficient in these
    # discarded experiments. Auditing their failure is not capture acceptance.
    experiments = {}
    for number in (5, 6):
        diagnostics = [event for event in rows(root, f"case{number}-native-events.jsonl")
                       if event["event"] == "hook_diagnostics"]
        assert any(event["armed"] for event in diagnostics)
        assert all(event["counters"] == [0] * 5 for event in diagnostics)
        experiments[str(number)] = "not_passed_no_callbacks"

    observer_ready = document(root, "observer-first-ready.json")
    observer = rows(root, "observer-first.jsonl")
    assert observer_ready["pid"] == ready["app"]
    assert observer_ready["system_keys_only"]
    assert any(event["vk"] == 9 and event["scan"] == 15
               and not event["injected"] for event in observer)
    assert all(event["foreground_thread"] == focus["thread"] for event in observer)
    assert document(root, "observer-first-stopped.json")["hook_removed"]
    assert document(root, "observer-second-stopped.json")["hook_removed"]
    assert rows(root, "observer-second.jsonl") == []
    stale_focus = document(root, "observer-second-stale-focus.json")
    assert datetime.fromisoformat(stale_focus["utc"]).timestamp() * 1000 < (
        document(root, "observer-second-ready.json")["unix_ms"])

    language = document(root, "language-verification.json")
    assert language["passed"] and language["actual"] == language["baseline"]
    assert language["owned_processes_remaining"] == []
    assert language["winrm_status"] == "Stopped"
    cleanup = document(root, "cleanup-independent.json")
    assert cleanup["passed"] and cleanup["share_absent"] and cleanup["language_restored"]
    for field in ("owned_tasks", "owned_processes", "owned_launchers", "owned_rules",
                  "test_listeners"):
        assert cleanup[field] == [], field
    assert (cleanup["winrm_status"], cleanup["winrm_start_mode"]) == ("Stopped", "Manual")
    assert {profile["Name"]: bool(profile["Enabled"])
            for profile in cleanup["firewall_profiles"]} == {
                "Domain": True, "Private": False, "Public": False}
    assert cleanup["languages"] == language["baseline"]

    old = document(root, "old-cleanup-independent.json")
    assert not old["endpoint_registry_present"]
    for field in ("owned_rules", "owned_tasks", "owned_processes", "test_listeners"):
        assert old[field] == [], field
    assert old["power_result"]["elapsed_ms"] > 30_000
    result = {
        "audit_passed": True,
        "accepted_native_key_records": counts,
        "native_system_capture_acceptance": "open_not_passed",
        "discarded_experiments": experiments,
        "independent_observer": "non_injected_tab_observed",
        "second_observer_matrix": "incomplete_empty_capture_stale_focus_not_accepted",
        "cleanup": "independently_verified",
        "suspend_resume": "no_automatic_30s_wake_proof",
        "vendor_text_consumption": "not_tested_by_synthetic_peer",
    }
    assert document(root, "analysis.json") == result, "stored analysis is stale"
    return result


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    print(json.dumps(audit(args.directory), indent=2, ensure_ascii=False))
