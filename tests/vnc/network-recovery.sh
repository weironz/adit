#!/usr/bin/env bash
# Owns only a unique namespace/veth pair, a synthetic VNC display and a scratch dir.
set -euo pipefail
[[ $(uname -s) == Linux ]] || { echo 'isolated Linux runner required' >&2; exit 2; }
[[ ! -e /tmp/.X114-lock ]] || { echo 'display :114 is already owned' >&2; exit 2; }
probe=$(realpath "${1:?pass the built vnc_recovery_probe binary}")
results=${2:?pass an owned results directory}
[[ ! -e "$results" ]] || { echo 'choose a fresh results directory; existing evidence is preserved' >&2; exit 2; }
mkdir -p "$results"
results=$(realpath "$results")
phase() { printf '%s %s\n' "$(date -u +%FT%TZ)" "$*" | tee -a "$results/runner.log"; }
work=$(mktemp -d /tmp/adit-vnc-net-XXXXXX)
ns="adit-vnc-net-$$"
host_a="adna$$"
host_b="adnb$$"
ns_created=0
server_pid=''
client_pid=''
cleanup() {
    local status=$?
    phase "cleanup exit=$status"
    if [[ "$ns_created" == 1 ]]; then
        for pid in $(sudo ip netns pids "$ns"); do
            sudo kill -CONT "$pid" 2>/dev/null || true
            sudo kill -TERM "$pid" 2>/dev/null || true
        done
        sudo ip netns delete "$ns" || true
    fi
    if [[ -n "$server_pid" ]]; then kill "$server_pid" 2>/dev/null || true; fi
    if [[ -n "$client_pid" ]]; then wait "$client_pid" 2>/dev/null || true; fi
    cp "$work"/*.log "$results/" 2>/dev/null || true
    cp "$work"/*.json "$results/" 2>/dev/null || true
    if [[ ! -f "$results/result.json" ]]; then
        printf '{"outcome":"failed","exit_code":%d,"scope":"isolated Linux network recovery; inspect retained logs"}\n' "$status" > "$results/result.json"
    fi
    # mktemp made this exact directory; no user-selected directory is deleted.
    sudo rm -rf -- "$work"
}
trap cleanup EXIT
trap 'exit 124' TERM INT
phase 'create isolated interfaces'
sudo -n true
sudo ip netns add "$ns"
ns_created=1
sudo ip link add "$host_a" type veth peer name nic_a netns "$ns"
sudo ip link add "$host_b" type veth peer name nic_b netns "$ns"
sudo ip addr add 10.203.0.1/30 dev "$host_a"
sudo ip addr add 10.204.0.1/30 dev "$host_b"
sudo ip link set "$host_a" up
sudo ip link set "$host_b" up
sudo ip -n "$ns" addr add 10.203.0.2/30 dev nic_a
sudo ip -n "$ns" addr add 10.204.0.2/30 dev nic_b
sudo ip -n "$ns" link set lo up
sudo ip -n "$ns" link set nic_a up
sudo ip -n "$ns" link set nic_b up

x11vnc -storepasswd adit-ci "$work/passwd" > /dev/null
phase 'start TigerVNC'
Xtigervnc :114 -noreset -geometry 800x600 -depth 24 -rfbport 15914 \
    -interface 10.203.0.1 -PasswordFile "$work/passwd" -SecurityTypes VncAuth \
    -localhost=0 > "$results/server.log" 2>&1 &
server_pid=$!
export DISPLAY=:114
for attempt in {1..10}; do
    kill -0 "$server_pid" || { cat "$results/server.log"; exit 1; }
    if timeout 2s xsetroot -solid red 2>/dev/null; then break; fi
    sleep 0.1
done
timeout 3s xsetroot -solid red
kill -0 "$server_pid"
phase 'start production client; wait for red frame and Fence'
printf '%s\n' adit-ci | sudo ip netns exec "$ns" "$probe" 10.203.0.1 15914 "$work" \
    > "$results/client.log" 2>&1 &
client_pid=$!
await_marker() {
    local name=$1
    local seconds=$2
    for ((attempt=0;attempt<seconds*5;attempt++)); do
        if [[ -f "$work/$name" ]]; then return; fi
        if ! kill -0 "$client_pid" 2>/dev/null; then
            cat "$results/client.log"
            echo "client exited before phase $name" >&2
            return 1
        fi
        sleep 0.2
    done
    cat "$results/client.log"
    echo "timed out waiting for phase $name" >&2
    return 1
}
await_marker ready 25
phase 'red frame/Fence ready; take nic_a down'
# This is a real interface outage in the client namespace, not a mocked RFB peer.
sudo ip -n "$ns" link set nic_a down
await_marker disconnected 50
phase 'old connection withdrawn; switch route to nic_b'
timeout 3s xsetroot -solid blue
sudo ip -n "$ns" route replace 10.203.0.1/32 via 10.204.0.1 dev nic_b src 10.204.0.2
sudo ip netns exec "$ns" ip route get 10.203.0.1 > "$work/route.log"
sudo ip netns exec "$ns" ip -j route get 10.203.0.1 > "$work/route.json"
sudo ip -j -n "$ns" link show dev nic_a > "$work/link.json"
await_marker recovered 35
phase 'blue frame recovered; verify actual source'
ss -ntH state established > "$work/sockets.log"
python3 - "$work" <<'PY'
import json,pathlib,sys
work=pathlib.Path(sys.argv[1])
route=json.loads((work/'route.json').read_text())[0]
link=json.loads((work/'link.json').read_text())[0]
assert route['dev']=='nic_b' and route['prefsrc']=='10.204.0.2',route
assert 'UP' not in link['flags'],link
assert any('10.203.0.1:15914' in line and '10.204.0.2:' in line
           for line in (work/'sockets.log').read_text().splitlines()),'server did not accept the new source'
PY

# Explicit process suspension is additional evidence, not an OS sleep test.
mapfile -t processes < <(sudo ip netns pids "$ns")
[[ ${#processes[@]} == 1 ]]
sudo kill -STOP "${processes[0]}"
phase 'pause production client'
for attempt in {1..50}; do
    if grep -q '^State:.*T' "/proc/${processes[0]}/status"; then break; fi
    sleep 0.02
done
grep -q '^State:.*T' "/proc/${processes[0]}/status"
pause_start=$(date +%s%N)
timeout 3s xsetroot -solid green
sleep 20
sudo kill -CONT "${processes[0]}"
pause_end=$(date +%s%N)
python3 - "$pause_start" "$pause_end" > "$work/pause.json" <<'PY'
import json,sys
seconds=(int(sys.argv[2])-int(sys.argv[1]))/1e9
assert seconds>=20,seconds
print(json.dumps({'observed_stopped_state':'T','seconds':seconds}))
PY
await_marker resumed 25
phase 'green frame received; wait for client exit'
wait "$client_pid"
client_pid=''
phase 'write acceptance result'
cp "$work"/*.log "$results/"
cp "$work"/route.json "$work"/link.json "$work"/pause.json "$results/"
python3 - "$probe" "$results" <<'PY'
import hashlib,json,os,pathlib,subprocess,sys
probe=pathlib.Path(sys.argv[1]); results=pathlib.Path(sys.argv[2])
record={
    'source_sha':os.environ.get('GITHUB_SHA','local-uncommitted'),
    'client_sha256':hashlib.sha256(probe.read_bytes()).hexdigest(),
    'os':subprocess.check_output(['uname','-sr'],text=True).strip(),
    'server':subprocess.check_output(['sh','-c','Xtigervnc -version 2>&1'],text=True).strip(),
    'network':'isolated client namespace; nic_a down, route switched to nic_b with a different source IPv4',
    'password':'disposable fixture credential; never a user credential',
    'outcome':'passed',
    'assertions':['same tab, one session','stale red desktop withdrawn','new blue full framebuffer after NIC change','VncAuth/ZRLE/read-only preserved','fresh green full framebuffer after 20-second process pause'],
    'limitations':['Linux namespace interfaces, not physical Wi-Fi hardware.','SIGSTOP/SIGCONT is process suspension, not OS suspend/hibernate.','Windows OS sleep and physical interface-switch acceptance remains separate.'],
    'phases':(results/'client.log').read_text(),
    'route':(results/'route.log').read_text(),
    'route_decision':json.loads((results/'route.json').read_text()),
    'failed_interface':json.loads((results/'link.json').read_text()),
    'process_pause':json.loads((results/'pause.json').read_text()),
}
(results/'result.json').write_text(json.dumps(record,indent=2)+'\n')
print(json.dumps(record,indent=2))
PY
