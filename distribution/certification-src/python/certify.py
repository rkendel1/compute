import json, os, platform, sys, time

op = sys.argv[1]
if op == "stdin":
    sys.stdout.buffer.write(sys.stdin.buffer.read())
elif op == "exit":
    sys.exit(7)
elif op == "sleep":
    time.sleep(5)
elif op == "certify":
    work = os.environ["COMPUTE_WORK_DIR"]
    output = os.environ["COMPUTE_OUTPUT_DIR"]
    result = {
        "input": open(os.path.join(work, "hello.txt"), encoding="utf-8").read(),
        "success": True,
        "argument": sys.argv[2],
        "environment": os.environ.get("CERTIFICATION_ENV", "missing"),
        "host_environment": os.environ.get("COMPUTE_HOST_SECRET", "missing"),
    }
    open(os.path.join(output, "result.json"), "w", encoding="utf-8").write(json.dumps(result, separators=(",", ":")))
    print(json.dumps({"runtime": "python", "runtime_version": platform.python_version()}, separators=(",", ":")))
    print("certification-stderr", file=sys.stderr)
