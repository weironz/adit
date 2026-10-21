"""Read-only native modifier audit; accepts witness or a later witness2 prefix.

Auditing a rejected run successfully does not promote it to final acceptance.
Only non-text modifier behavior and captured-system-key order are in scope.
"""

import argparse
import hashlib
import json
from pathlib import Path
import re


AG, CL, CR, WL, WR = 0xFE03, 0xFFE3, 0xFFE4, 0xFFEB, 0xFFEC
CORE12 = [(WL, True), (0x72, True), (0x72, False), (WL, False),
          (WR, True), (0x65, True), (0x65, False), (WR, False),
          (0xFFE9, True), (0xFF09, True), (0xFF09, False), (0xFFE9, False)]
CTRL_ESC = [(CL, True), (0xFF1B, True), (0xFF1B, False), (CL, False),
            (CR, True), (0xFF1B, True), (0xFF1B, False), (CR, False)]


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def read_json(path):
    return json.loads(Path(path).read_text(encoding="utf-8-sig"))


def read_jsonl(path):
    return [json.loads(line) for line in Path(path).read_text(encoding="utf-8-sig").splitlines() if line.strip()]


def key_records(records):
    return [e for e in records if e.get("event") == "key"]


def edges(records):
    return [(e["keysym"], e["down"]) for e in key_records(records)]


def down_set(records):
    held = set()
    for key, down in edges(records):
        if down:
            require(key not in held, f"Unexpected duplicate down: {key:#x}")
            held.add(key)
        else:
            require(key in held, f"Unowned release: {key:#x}")
            held.remove(key)
    return held


def appended(before, after, label):
    require(after[:len(before)] == before, f"{label}: snapshots are not append-only")
    return after[len(before):]


def last_event(records, name, at=None):
    matches = [e for e in records if e.get("event") == name and (at is None or e["unix_ms"] <= at)]
    require(bool(matches), f"Missing {name} event")
    return matches[-1]


def observer_edge(event, scan, extended, down):
    return (event.get("scan") == scan and bool(event["flags"] & 1) == extended
            and event["message"] in ((0x100, 0x104) if down else (0x101, 0x105)))


def summarize_ci(text):
    groups = {name: {"passed": 0, "failed": 0, "ignored": 0, "summaries": 0} for name in ("root", "feature", "graphics")}
    group, ui_binaries = "root", 0
    for line in text.splitlines():
        if "Running unittests" in line and "adit_ui-" in line:
            ui_binaries += 1
            if ui_binaries == 2:
                group = "feature"
        if "Running unittests" in line and "ironrdp_graphics-" in line:
            group = "graphics"
        match = re.match(r"test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored;", line)
        if match:
            status, passed, failed, ignored = match.groups()
            require(status == "ok" and int(failed) == 0, "CI test failure")
            for key, count in (("passed", passed), ("failed", failed), ("ignored", ignored)):
                groups[group][key] += int(count)
            groups[group]["summaries"] += 1
    require(ui_binaries == 2 and all(g["summaries"] > 0 for g in groups.values()), "CI root/feature/graphics grouping")
    return groups


def audit_cleanup(directory, manifest):
    names = ("witness-independent-before-cleanup.json", "witness-language-baseline.json", "witness-language-restored.json", "witness-cleanup-reports.json", "witness2-native-stopped.json", "witness-control-final.json")
    listed = {e["path"] for e in manifest["files"]}
    require(all(f"cleanup/{name}" in listed for name in names), "Cleanup evidence missing/unmanifested")
    load = lambda name: read_json(directory / "cleanup" / name)
    as_list = lambda value: value if isinstance(value, list) else [value]
    baseline = as_list(load("witness-language-baseline.json"))
    restored = as_list(load("witness-language-restored.json"))
    before = load("witness-independent-before-cleanup.json")
    reports = load("witness-cleanup-reports.json")
    require(len(reports) == 2, "Two cleanup stages required")
    owned, final = reports
    require(owned["stage"] == "owned_cleanup" and final["stage"] == "owned_cleanup_final", "Cleanup stage identities")
    require(before["stage"] == "independent_before_cleanup" and before["language_restored"] is True, "Independent language recovery")
    require(baseline == restored == as_list(before["baseline"]) == as_list(before["languages"]) == as_list(final["languages"]), "Complete language baseline mismatch")
    require(len(baseline) == 1 and baseline[0]["LanguageTag"] == "zh-Hans-CN", "Original language baseline identity")
    require(before["owned_processes"] == before["listeners"] == [], "Before-cleanup fixture resources remain")
    require(owned["remaining_processes"] == owned["tasks_remaining"] == [], "Owned cleanup resources remain")
    require(final["processes"] == final["tasks"] == final["listeners"] == [], "Final fixture resources remain")
    require(owned["share_absent"] is True and final["share_absent"] is True and final["directory_absent"] is True, "Owned share/directory remain")
    require(owned["directory"] == final["directory"] and re.fullmatch(r"C:\\ProgramData\\AditVncKeys-[a-f0-9]{32}", final["directory"]), "Exact owned directory")
    require(before["firewall"] == owned["firewall"] == final["firewall"], "Firewall changed during cleanup")
    require(before["winrm"] == owned["winrm"] == final["winrm"] == "Stopped", "WinRM baseline changed")
    require(load("witness2-native-stopped.json")["owned_processes_stopped"] is True, "Native processes not stopped")
    control = load("witness-control-final.json")
    require(control["driver_pid"] == 3004 and control["driver_exit"] == control["helper_exit_code"] == 0, "Final native control shutdown")
    require(control["capture_enabled"] is False and control["helper_exit_source"] == ["helper_exit=exit code: 0\n"], "Final native helper exit provenance")
    require(control["helper_sha256"] == "7ae9637ea26e09837da78ae45b7f2028cf320c8cb8687537c7362a7c18cd3043", "Final native helper identity")
    require(before["utc"] < owned["utc"] < final["utc"], "Cleanup chronology")
    require(all(task["TaskName"].startswith(final["directory"].split("\\")[-1] + "-") for task in before["tasks"]), "Non-owned task baseline")
    return {"language_full_baseline_restored": True, "directory_absent": True, "share_absent": True,
            "owned_processes_tasks_listeners_absent": True, "firewall_unchanged": True,
            "winrm_stopped": True, "final_native_control_helper_exit_zero": True,
            "earlier_clearcodec_capture_exit_reclassified": False, "evidence_is_saved_vm_reports_not_new_live_probe": True}


def check_handshake(directory, prefix, case, app, observer, control=None, require_monotonic=False):
    handshake = read_json(directory / f"{prefix}-{case}-handshake.json")
    require(handshake["label"] == f"{prefix}-{case}", f"{case}: handshake label")
    require(handshake["exit_code"] == 0, f"{case}: handshake process exit")
    observed = [e for e in handshake["events"] if e["event"] == "observed"]
    require(len(observed) == 2, f"{case}: expected start/end handshake")
    start, end = [e["record"] for e in observed]
    require(start["event"] == "ui_stall_start" and end["event"] == "ui_stall_end", f"{case}: event order")
    require(start in app and end in app, f"{case}: handshake not present in app log")
    require(start["sequence"] == end["sequence"], f"{case}: stall sequence")
    require(start["sequence"] > handshake["events"][0]["prior_sequence"], f"{case}: reused stall")
    require(start["trigger"] == "F8" and start["duration_ms"] == 1500, f"{case}: native F8 injection")
    require(end["elapsed_ms"] >= 1500 and end["unix_ms"] - start["unix_ms"] >= 1500, f"{case}: UI stall too short")
    require(start["grab"] is True and end["grab"] is True, f"{case}: capture disabled")
    require(start["input_epoch"] == end["input_epoch"], f"{case}: epoch retired")
    for event in (start, end):
        require(all(event["focus"][k] is True for k in ("active_owned", "focus_owned", "foreground_owned", "gui_thread_info_ok")), f"{case}: focus")
    threads = start["threads"]
    require(threads["same"] is True and threads["dispatcher"] == threads["foreground"] != 0, f"{case}: stall not on foreground UI thread")
    prior = last_event(app, "hook_diagnostics", start["unix_ms"])
    require(prior["armed"] is True and prior["vnc"] is True, f"{case}: hook not armed")
    require(prior["worker_thread"] and prior["worker_thread"] != threads["dispatcher"], f"{case}: hook not independently pumping")
    ime = last_event(app, "native_ime_state", start["unix_ms"])["state"]
    require(ime["foreground"] is True and ime["grab"] is True and ime["layout"] & 0xFFFF == 0x0407, f"{case}: foreground German layout")
    commands = {e["stage"]: e["unix_ms"] for e in handshake["commands"]}
    require(commands["trigger"] < commands["burst"], f"{case}: trigger/burst command order")
    require(observed[0]["received_unix_ms"] < observed[1]["received_unix_ms"], f"{case}: handshake observation order")
    if require_monotonic:
        mono_commands = {e["stage"]: e["monotonic_ms"] for e in handshake["commands"]}
        receipt = observed[0]["node_receipt_monotonic_ms"]
        receipt_end = observed[1]["node_receipt_monotonic_ms"]
        require(all(isinstance(v, (int, float)) for v in (*mono_commands.values(), receipt, receipt_end)), f"{case}: monotonic values")
        require(mono_commands["trigger"] <= receipt <= mono_commands["burst"] <= receipt_end, f"{case}: Node callback/write monotonic causal order")
    # Controller receipt/command timestamps can be sampled in separate async
    # paths. One preserved run has receipt 1 ms later than burst. Do not invent
    # clock tolerance or claim timestamp-level causality: the strict backlog
    # proof below uses app and observer timestamps from the same VM instead.
    required = [(0x38, True, True)]
    if control is not None:
        required.insert(0, (0x1D, control, True))
    real = []
    for scan, extended, down in required:
        candidates = [e for e in observer if start["unix_ms"] <= e["unix_ms"] <= end["unix_ms"] and observer_edge(e, scan, extended, down)]
        require(len(candidates) == 1, f"{case}: exact real LL edge absent/duplicated inside stall")
        edge = candidates[0]
        require(edge["injected"] is False and not (edge["flags"] & 0x10), f"{case}: injected observer edge")
        require(edge["foreground_thread"] == threads["foreground"], f"{case}: LL foreground thread")
        real.append(edge)
    require([e["unix_ms"] for e in real] == sorted(e["unix_ms"] for e in real), f"{case}: LL edge order")
    burst = "KEY 38 1 1"
    if control is not None:
        middle = "" if case == "preheld-win" else "\nKEY 5b 1 1\nKEY 5b 1 0"
        burst = f"KEY 1d {int(control)} 1{middle}\nKEY 38 1 1"
    require(handshake["burst"] == burst, f"{case}: burst command order")
    command_file = (directory / f"{prefix}-{case}-burst.commands.txt").read_text(encoding="utf-8-sig")
    require(command_file == f"KEY 42 0 1\n[await real ui_stall_start]\nKEY 42 0 0\n{burst}\n", f"{case}: command evidence differs")
    after = last_event(app, "hook_diagnostics")
    swallowed = after["counters"][4] - prior["counters"][4]
    expected_swallowed = 2 if case in ("left-control", "right-control") else 0
    require(swallowed == expected_swallowed, f"{case}: captured-key counter delta")
    # Production hook blocks Win from downstream observers. Command order,
    # swallowed counter and peer order prove this portion; no Win LL timestamp
    # is inferred from the two visible Ctrl/RightAlt observer timestamps.
    return {"sequence": start["sequence"], "start_ms": start["unix_ms"], "end_ms": end["unix_ms"],
            "visible_ll_ms": [e["unix_ms"] for e in real], "captured_key_count": swallowed,
            "win_ll_timestamps_claimed": False, "epoch": start["input_epoch"],
            "receipt_minus_burst_ms": observed[0]["received_unix_ms"] - commands["burst"],
            "receipt_timestamp_proves_causal_gate": False,
            "node_monotonic_causal_gate_verified": require_monotonic}


def audit_matrix(directory, prefix="witness", expected_core="rejected"):
    directory = Path(directory)
    require(expected_core in ("rejected", "passed"), "Expected core classification")
    manifest_path = directory / "provenance.json"
    require(manifest_path.is_file(), "Provenance manifest required")
    manifest = read_json(manifest_path)
    require(manifest["prefix"] == prefix, "Manifest prefix")
    require(manifest["status"] == ("rejected-incomplete" if expected_core == "rejected" else "candidate"), "Manifest must agree with acceptance status")
    listed = set()
    for entry in manifest["files"]:
        relative = Path(entry["path"])
        require(not relative.is_absolute() and ".." not in relative.parts, "Unsafe manifest path")
        require(entry["path"] not in listed, "Duplicate manifest entry")
        listed.add(entry["path"])
        data = (directory / relative).read_bytes()
        require(len(data) == entry["bytes"] and hashlib.sha256(data).hexdigest() == entry["sha256"], f"{relative}: provenance mismatch")
    for case in ("pure-altgr", "left-control", "right-control", "preheld-win"):
        for suffix in ("handshake.json", "burst.commands.txt"):
            require(f"{prefix}-{case}-{suffix}" in listed, f"{case}: unmanifested handshake/commands")
    build_provenance = None
    if expected_core == "passed":
        for suffix in ("ci-vnc.txt", "native-build.txt", "native-ready.json", "languages.json", "preheld-off.commands.txt"):
            require(f"{prefix}-{suffix}" in listed, f"Missing build/runtime provenance: {suffix}")
        ready = read_json(directory / f"{prefix}-native-ready.json")
        binary = next((b for b in manifest["binary_observations"] if b["private_source_name"] == f"{prefix}-native.exe"), None)
        require(binary is not None and binary["binary_copied"] is False, "Runtime binary observation")
        require(binary["sha256"] == ready["source_binary_sha256"].lower(), "Runtime ready/binary SHA differs")
        require(ready["app"] > 0 and ready["observer"] > 0 and ready["fixture"] > 0 and ready["session"] > 0, "Interactive runtime identities")
        source_files = [e for e in manifest["files"] if e.get("kind") == "current-frozen-source-snapshot"]
        require(len(source_files) == 10 and all(e["path"].startswith("source/crates/") and e["path"].endswith(".rs") for e in source_files), "Ten frozen Rust source files required")
        ci = (directory / f"{prefix}-ci-vnc.txt").read_text(encoding="utf-8-sig")
        summaries = re.findall(r"^test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored;", ci, re.MULTILINE)
        require(bool(summaries) and all(status == "ok" and int(failed) == 0 for status, _, failed, _ in summaries), "CI test summary failure/missing")
        require(re.search(r"^error(?:\[|:)", ci, re.MULTILINE) is None, "CI compile error")
        build = (directory / f"{prefix}-native-build.txt").read_text(encoding="utf-8-sig")
        require("Finished `dev` profile" in build and "Compiling adit-ui" in build, "Native fixture build provenance")
        require(re.fullmatch(r"[a-f0-9]{40}", manifest["source_commit"]), "Subsequent source commit identity")
        build_provenance = {"source_base": manifest["source_base"], "source_state": manifest["source_state"], "source_commit": manifest["source_commit"],
                            "source_snapshots": len(source_files), "native_binary_sha256": binary["sha256"],
                            "native_app_pid": ready["app"], "ci_successful_test_summaries": len(summaries),
                            "ci_groups": summarize_ci(ci), "ci_groups_overlap": "Feature UI run overlaps the root UI suite; do not sum as unique tests",
                            "cleanup_claimed": True}
        require(f"{prefix}-ci-counts.json" in listed, "Independent CI command-group counts missing")
        independent_ci = read_json(directory / f"{prefix}-ci-counts.json")
        require(independent_ci["exit_code"] == 0 and independent_ci["counts_overlap"] is True and independent_ci["hosted_ci_run"] is False, "Independent local CI result")
        name_map = {"root": "root", "native_feature": "feature", "vendor_graphics": "graphics"}
        require(len(independent_ci["groups"]) == 3, "Independent CI groups")
        for group in independent_ci["groups"]:
            computed = build_provenance["ci_groups"][name_map[group["name"]]]
            require(all(group[k] == computed[k] for k in ("passed", "failed", "ignored")), "Independent/computed CI counts differ")
    def snapshot(stage, kind):
        name = f"{prefix}-{stage}-{kind}.jsonl"
        require(name in listed, f"Unmanifested snapshot: {name}")
        return read_jsonl(directory / name)
    stages = ("pure-altgr", "left-held", "left-after-alt", "left-final", "right-held", "right-after-alt", "right-final", "preheld-start", "preheld-end", "core-start", "core")
    data = {stage: {kind: snapshot(stage, kind) for kind in ("app", "peer", "observer")} for stage in stages}
    # Core may precede the four stalls in a fresh run. Cumulative snapshot
    # lengths establish chronology; compare complete records, never assume
    # peer key zero belongs to the AltGr case.
    chronology = sorted(stages, key=lambda stage: len(data[stage]["app"]))
    for previous, current in zip(chronology, chronology[1:]):
        for kind in ("app", "peer", "observer"):
            appended(data[previous][kind], data[current][kind], f"{previous}->{current}/{kind}")
    report = {"prefix": prefix, "classification": "rejected-incomplete" if expected_core == "rejected" else "candidate",
              "scope": "Native modifier ownership/captured-system-key relative order; not all ordinary WM text global order", "snapshot_chronology": chronology, "stalls": {}}
    if build_provenance is not None:
        report["build_provenance"] = build_provenance
        report["cleanup"] = audit_cleanup(directory, manifest)
    pure = data["pure-altgr"]
    report["stalls"]["pure-altgr"] = check_handshake(directory, prefix, "pure-altgr", pure["app"], pure["observer"], require_monotonic=expected_core == "passed")
    start = report["stalls"]["pure-altgr"]["start_ms"]
    pure_keys = [e for e in key_records(pure["peer"]) if e["unix_ms"] >= start]
    require(edges(pure_keys) == [(AG, True), (0x40, True), (0x40, False), (AG, False)], "Pure AltGr leaked Ctrl or wrong text/wire")
    require(down_set(pure["peer"]) == set(), "Pure AltGr unbalanced")
    for side, symbol, extended, previous in (("left", CL, False, "pure-altgr"), ("right", CR, True, "left-final")):
        held, after, final = [data[f"{side}-{stage}"] for stage in ("held", "after-alt", "final")]
        report["stalls"][side] = check_handshake(directory, prefix, f"{side}-control", held["app"], held["observer"], extended, expected_core == "passed")
        delta = appended(data[previous]["peer"], held["peer"], f"{side} held")
        require(edges(delta) == [(symbol, True), (WL, True), (WL, False), (AG, True)], f"{side}: Ctrl/Key*/Alt wire order")
        require(down_set(held["peer"]) == {symbol, AG}, f"{side}: held down-set")
        require(edges(appended(held["peer"], after["peer"], side)) == [(AG, False)], f"{side}: AltGr up invented Ctrl release")
        require(down_set(after["peer"]) == {symbol}, f"{side}: true Ctrl released too soon")
        require(edges(appended(after["peer"], final["peer"], side)) == [(symbol, False)], f"{side}: real Ctrl release")
        require(down_set(final["peer"]) == set(), f"{side}: final unbalanced")
        obs_delta = appended(held["observer"], after["observer"], side)
        require(any(observer_edge(e, 0x38, True, False) for e in obs_delta), f"{side}: real Alt release missing")
        require(not any(observer_edge(e, 0x1D, extended, False) for e in obs_delta), f"{side}: Ctrl was physically released during Alt-up stage")
        release_obs = appended(after["observer"], final["observer"], side)
        require(any(observer_edge(e, 0x1D, extended, False) for e in release_obs), f"{side}: real Ctrl release missing")
    pre, post = data["preheld-start"], data["preheld-end"]
    report["stalls"]["preheld-win"] = check_handshake(directory, prefix, "preheld-win", post["app"], post["observer"], False, expected_core == "passed")
    require(edges(appended(pre["peer"], post["peer"], "preheld Win")) == [], "Preheld local Win leaked remote keys")
    require(down_set(pre["peer"]) == down_set(post["peer"]) == set(), "Preheld Win remote down-set")
    stamp = report["stalls"]["preheld-win"]["start_ms"]
    wins = [e for e in pre["observer"] if observer_edge(e, 0x5B, True, True) and e["unix_ms"] < stamp]
    require(bool(wins), "No preheld real Win")
    win = wins[-1]
    require(last_event(post["app"], "hook_diagnostics", win["unix_ms"])["armed"] is False, "Win was not pressed before hook arm")
    require(last_event(post["app"], "native_ime_state", win["unix_ms"])["state"]["grab"] is False, "Win was not pressed before grab")
    arm = last_event(post["app"], "grab_state", stamp)
    require(arm["enabled"] is True and win["unix_ms"] < arm["unix_ms"], "Preheld Win arm order")
    require(not any(observer_edge(e, 0x5B, True, False) and e["unix_ms"] > win["unix_ms"] for e in post["observer"]), "Preheld Win released during case")
    core_before, core = data["core-start"], data["core"]
    core_delta = appended(core_before["peer"], core["peer"], "core")
    expected = CORE12 if expected_core == "rejected" else CORE12 + CTRL_ESC
    require(edges(core_delta) == expected, "Core wire mismatch (WinR/rightWinE/AltTab/CtrlEsc)")
    require(down_set(core_before["peer"]) == down_set(core["peer"]) == set(), "Core unbalanced")
    last_hook = last_event(core["app"], "hook_diagnostics")
    baseline_hook = last_event(core_before["app"], "hook_diagnostics")
    require(last_hook["counters"][4] - baseline_hook["counters"][4] == (10 if expected_core == "rejected" else 14), "Core captured-key counter delta")
    last_ime = last_event(core["app"], "native_ime_state")["state"]
    if expected_core == "rejected":
        require(last_hook["armed"] is False and last_ime["foreground"] is False, "Rejected core no longer matches recorded lost capture/focus")
        core_obs = appended(core_before["observer"], core["observer"], "core observer")
        require(any(observer_edge(e, 0x1D, False, True) for e in core_obs), "Rejected core Ctrl-down missing")
        require(any(observer_edge(e, 0x01, False, True) for e in core_obs), "Rejected core Escape-down missing")
    else:
        require(last_hook["armed"] is True and last_hook["vnc"] is True, "Final core capture not armed")
        require(last_ime["foreground"] is True and last_ime["grab"] is True, "Final core foreground/capture lost")
    require(not any(e["keysym"] == 0xFFC5 for stage in data.values() for e in key_records(stage["peer"])), "Fixture F8 leaked to peer")
    report["core_wire_events"] = len(edges(core_delta))
    require(len({s["sequence"] for s in report["stalls"].values()}) == 4, "Stall sequences reused")
    report["core_ctrl_escape"] = "incomplete" if expected_core == "rejected" else "passed"
    report["witness_branch_execution_claimed"] = False
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", nargs="?", type=Path, default=Path(__file__).resolve().parent / "rejected")
    parser.add_argument("--prefix", default="witness")
    parser.add_argument("--expected-core", choices=("rejected", "passed"), default="rejected")
    args = parser.parse_args()
    report = audit_matrix(args.directory, args.prefix, args.expected_core)
    print(json.dumps(report, ensure_ascii=True, indent=2))
    print("Evidence audit passed; classification remains " + report["classification"] + ".")


if __name__ == "__main__":
    main()
