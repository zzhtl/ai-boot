"""隔离工程验收：真实本地 Git/制品/部署目录，GitLab/Jenkins 使用模拟接口。"""
import hashlib
import json
import pathlib
import subprocess
import sys
import tarfile

root = pathlib.Path(sys.argv[-1])
remote = root / "remote.git"
state_file = root / "state.json"


def run(*args, cwd=None):
    return subprocess.check_output(args, cwd=cwd, text=True).strip()


if sys.argv[1] == "--init":
    root.mkdir(parents=True, exist_ok=True)
    seed = root / "seed"
    run("git", "init", "--bare", str(remote))
    run("git", "init", "-b", "release/1.2", str(seed))
    run("git", "config", "user.name", "Engineering Test", cwd=seed)
    run("git", "config", "user.email", "test@example.test", cwd=seed)
    (seed / "app.py").write_text("import sys\nprint(int(sys.argv[1]) - int(sys.argv[2]))\n")
    (seed / "test_app.py").write_text(
        "import subprocess,sys,unittest\nclass Addition(unittest.TestCase):\n"
        " def test_add(self):\n"
        "  for a,b in [(2,3),(-2,3),(0,0)]:\n"
        "   self.assertEqual(subprocess.check_output([sys.executable,'app.py',str(a),str(b)],text=True).strip(),str(a+b))\n"
    )
    (seed / "README.md").write_text("产品 1.2.3 对应 v1.2.3，MR 目标 release/1.2。\n")
    (seed / "AGENTS.md").write_text(
        "产品 1.2.3 的基线 tag 是 v1.2.3，MR 目标 release/1.2。用 python3 -m unittest 验证。\n"
        "在隔离 clone 中修改；仅暂存相关文件。分支按 fix/LAB-42 命名，已有同任务分支先查询。\n"
        "构建和发布沿用 qtmcp 提供的 lab/package 与 lab/deploy 任务，先查 job 参数。\n"
        "验收需要执行实际部署目录中的 app.py，不能只信 Jenkins SUCCESS。\n"
    )
    run("git", "add", "app.py", "test_app.py", "README.md", "AGENTS.md", cwd=seed)
    run("git", "commit", "-m", "chore: baseline 1.2.3", cwd=seed)
    base = run("git", "rev-parse", "HEAD", cwd=seed)
    run("git", "tag", "v1.2.3", cwd=seed)
    run("git", "switch", "-c", "main", cwd=seed)
    (seed / "app.py").write_text("import sys\nprint(int(sys.argv[1]) * int(sys.argv[2]))\n")
    run("git", "add", "app.py", cwd=seed)
    run("git", "commit", "-m", "chore: unrelated newer main", cwd=seed)
    run("git", "remote", "add", "origin", str(remote), cwd=seed)
    run("git", "push", "origin", "main", "release/1.2", "--tags", cwd=seed)
    run("git", "symbolic-ref", "HEAD", "refs/heads/main", cwd=remote)
    (seed / "README.md").write_text("已有未提交改动，请保留。\n")
    state_file.write_text(json.dumps({"base": base, "mrs": [], "builds": [], "writes": []}))
    sys.exit(0)


def tool(name, args):
    state = json.loads(state_file.read_text())
    action = args.get("action")
    if name == "gitlab_project":
        return {"project": "lab/demo", "clone_url": str(remote), "default_branch": "main", "branches": ["main", "release/1.2"], "version_tag": "v1.2.3", "baseline_sha": state["base"]}
    if name == "gitlab_mr":
        if action == "list":
            return state["mrs"]
        if action == "create":
            assert not state["mrs"], "同一任务已有 MR，不能重复创建"
            assert args["target_branch"] == "release/1.2", "版本目标分支错误"
            assert args["source_branch"] == "fix/LAB-42", "任务分支错误"
            assert args["title"].startswith("[LAB-42] "), "MR 缺少工单前缀"
            commit = run("git", "rev-parse", args["source_branch"], cwd=remote)
            assert run("git", "rev-parse", commit + "^", cwd=remote) == state["base"], "没有从指定版本基线修改"
            state["mrs"].append({**args, "iid": 1, "sha": commit, "state": "opened", "web_url": "https://git.example.test/lab/demo/-/merge_requests/1"})
        else:
            return state["mrs"][0]
    elif name == "jenkins_job":
        return {"job": args["job"], "parameters": ["COMMIT"] if args["job"] == "lab/package" else ["ARTIFACT", "ENVIRONMENT"], "environment": "isolated-test", "deploy_dir": str(root / "deployed"), "side_effect": "package_only" if args["job"] == "lab/package" else "deploy_to_isolated_directory"}
    elif name == "jenkins_build":
        if action != "trigger":
            return next(b for b in state["builds"] if str(b["number"]) == args.get("build") and b["job"] == args.get("job")) if action == "get" else state["builds"]
        job, params = args["job"], args.get("params", {})
        number = len(state["builds"]) + 1
        build = {"job": job, "number": number, "queue_id": number, "url": f"https://jenkins.example.test/job/{job}/{number}/", "building": False, "result": "SUCCESS"}
        if job == "lab/package":
            commit = params["COMMIT"]
            assert commit == state["mrs"][0]["sha"], "构建提交与 MR 不一致"
            build["commit"] = commit
            if (root / "fail_next_package").exists():
                build.update(result="FAILURE", log="测试构建失败：不得继续部署")
            else:
                artifact = root / f"package-{commit}.tar.gz"
                subprocess.run(["git", "archive", "--format=tar.gz", "--output=" + str(artifact), commit], cwd=remote, check=True)
                build.update(artifact=str(artifact), sha256=hashlib.sha256(artifact.read_bytes()).hexdigest())
        elif job == "lab/deploy":
            assert params["ENVIRONMENT"] == "isolated-test", "环境错误"
            package = next(b for b in reversed(state["builds"]) if b["job"] == "lab/package")
            assert package["result"] == "SUCCESS", "失败制品不能部署"
            assert params["ARTIFACT"] == package["artifact"], "部署制品错误"
            deployed = root / "deployed"
            deployed.mkdir(exist_ok=True)
            with tarfile.open(package["artifact"]) as archive:
                archive.extractall(deployed, filter="data")
            (deployed / "version.json").write_text(json.dumps({"commit": package["commit"], "sha256": package["sha256"]}))
            build.update(commit=package["commit"], sha256=package["sha256"], environment="isolated-test", deploy_dir=str(deployed))
        else:
            raise ValueError("未授权的 job")
        state["builds"].append(build)
    else:
        raise ValueError("未知工具")
    state["writes"].append({"tool": name, **args})
    state_file.write_text(json.dumps(state))
    return state["mrs"][-1] if name == "gitlab_mr" else state["builds"][-1]


properties = {key: {"type": "string"} for key in ["action", "project", "branch", "from_ref", "source_branch", "target_branch", "title", "body", "job", "build"]}
properties.update(iid={"type": "integer"}, params={"type": "object"}, patch={"type": "object"})
for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    method = request.get("method")
    if method == "initialize":
        result = {"protocolVersion": request["params"]["protocolVersion"], "capabilities": {"tools": {}}, "serverInfo": {"name": "isolated-engineering", "version": "1"}}
    elif method == "tools/list":
        result = {"tools": [{"name": name, "description": description, "inputSchema": {"type": "object", "properties": properties, "required": ["action"]}} for name, description in [("gitlab_project", "查询项目、版本基线和 clone 地址"), ("gitlab_mr", "MR list/get/create/update/changes"), ("jenkins_job", "job get/config 查询参数和发布方式"), ("jenkins_build", "trigger/get/list/log/queue；按明确 build 号查询")]]}
    elif method == "tools/call":
        try:
            state = json.loads(state_file.read_text())
            state.setdefault("calls", []).append(request["params"])
            state_file.write_text(json.dumps(state))
            value = tool(request["params"]["name"], request["params"].get("arguments", {}))
            result = {"content": [{"type": "text", "text": json.dumps(value, ensure_ascii=False)}], "isError": False}
        except Exception as error:
            result = {"content": [{"type": "text", "text": str(error)}], "isError": True}
    else:
        result = {}
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
