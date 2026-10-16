"""Run only against the disposable loopback-bound Docker matrix. Python 3 stdlib."""
import argparse, json, pathlib, re, subprocess, time
parser=argparse.ArgumentParser()
parser.add_argument("--probe",required=True)
parser.add_argument("--container",default="adit-vnc-matrix")
parser.add_argument("--samples",type=int,default=10)
parser.add_argument("--output",type=pathlib.Path,required=True)
parser.add_argument("--first-screen-deadline",type=int,default=15)
parser.add_argument("--steady-seconds",type=int,default=5)
parser.add_argument("--network",choices=["all","loopback","wan"],default="all")
args=parser.parse_args()
servers=[("x11vnc",15902,99),("TigerVNC",15903,100),("TurboVNC",15904,101)]
def docker(*cmd):
    return subprocess.run(["docker","exec",args.container,*cmd],capture_output=True,text=True,encoding="utf-8",check=True).stdout
records=[]
networks={"all":["loopback","180ms-5mbit-0.1pct"],"loopback":["loopback"],"wan":["180ms-5mbit-0.1pct"]}
for network in networks[args.network]:
    if network=="loopback":
        subprocess.run(["docker","exec",args.container,"tc","qdisc","del","dev","eth0","root"],capture_output=True)
    else:
        docker("tc","qdisc","replace","dev","eth0","root","netem","delay","180ms","rate","5mbit","loss","0.1%","limit","10000")
    for name,port,display in servers:
        for quality in ["lossless","wan"]:
            for workload in ["static","scroll","drag"]:
                for index in range(1,args.samples+1):
                    docker("touch",f"/tmp/adit-reset-{display}")
                    for _ in range(100):
                        if docker("sh","-c",f"test -e /tmp/adit-reset-{display} && echo waiting || true").strip()!="waiting": break
                        time.sleep(.02)
                    else: raise RuntimeError("fixture did not acknowledge reset")
                    cmd=[str(pathlib.Path(args.probe).resolve()),"127.0.0.1",str(port),str(args.first_screen_deadline),f"--steady-seconds={args.steady_seconds}",f"--workload={workload}"]
                    if quality=="wan":cmd.append("--wan")
                    result=subprocess.run(cmd,input="adit-ci\n",capture_output=True,text=True,encoding="utf-8",timeout=args.first_screen_deadline+args.steady_seconds+10)
                    record={"server":name,"network":network,"quality":quality,"workload":workload,"sample":index,"sampling_mode":"fixed_steady_after_first_coverage","first_screen_deadline_s":args.first_screen_deadline,"steady_target_s":args.steady_seconds,"exit":result.returncode,"stdout":result.stdout,"stderr":result.stderr}
                    for key in ["connected_ms","first_paint_ms","complete_paint_ms","sample_ms","steady_ms"]:
                        found=re.search(rf"{key}=(\d+)",result.stdout)
                        record[key]=int(found[1]) if found else None
                    for key in ["wire_bytes","complete_updates","queue_high_water","pending_updates","painted_pixels"]:
                        found=re.search(rf"{key}: (\d+)",result.stdout)
                        record[key]=int(found[1]) if found else None
                    record["failure_kind"] = None if result.returncode==0 else ("first_screen_timeout" if "first_screen_timeout_s=" in result.stderr else "transport_or_protocol_error")
                    records.append(record)
                    args.output.parent.mkdir(parents=True,exist_ok=True)
                    args.output.write_text(json.dumps(records,ensure_ascii=False,indent=2),encoding="utf-8")
                    print(json.dumps({k:record[k] for k in ["server","network","quality","workload","sample","exit","complete_paint_ms","wire_bytes"]}),flush=True)
                    if result.returncode: raise RuntimeError("probe failed; raw evidence saved; fix before continuing")
# Always leave the disposable container with its measured qdisc visible for audit;
# the documented cleanup removes the container rather than changing a host NIC.
