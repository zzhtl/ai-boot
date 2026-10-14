"""内网优先查询验证：搜索摘要含过期示例，正文提供正确版本；另提供缺公共证据的场景。"""
import json
import pathlib
import sys

calls = pathlib.Path(sys.argv[1])
for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    method = request.get("method")
    if method == "initialize":
        result = {"protocolVersion": request["params"]["protocolVersion"], "capabilities": {"tools": {}}, "serverInfo": {"name": "internal-query-probe", "version": "1"}}
    elif method == "tools/list":
        result = {"tools": [
            {"name": "confluence_search", "description": "CQL 搜索内网页面，只返回标题、摘要、ID 与 URL；正文另查 confluence_page。", "inputSchema": {"type": "object", "properties": {"cql": {"type": "string"}, "limit": {"type": "integer"}}, "required": ["cql"]}, "annotations": {"readOnlyHint": True}},
            {"name": "confluence_page", "description": "读取 Confluence storage 原文；action=get/by_title，使用 page_id 或 title；max_chars 默认 15000。", "inputSchema": {"type": "object", "properties": {"action": {"type": "string"}, "page_id": {"type": "string"}, "title": {"type": "string"}, "max_chars": {"type": "integer"}, "offset": {"type": "integer"}}, "required": ["action"]}, "annotations": {"readOnlyHint": True}},
        ]}
    elif method == "tools/call":
        name = request["params"]["name"]
        args = request["params"].get("arguments", {})
        with calls.open("a") as output:
            output.write(json.dumps({"tool": name, "args": args}, ensure_ascii=False) + "\n")
        if name == "confluence_search":
            public_gap = "429" in args.get("cql", "") or "网关" in args.get("cql", "")
            value = {"results": [{"id": "202" if public_gap else "101", "title": "内网网关响应记录" if public_gap else "3.x K3s Kafka 消费积压查询", "url": "https://wiki.example.test/pages/" + ("202" if public_gap else "101"), "excerpt": "只记录现象，未说明 HTTP 429 或 Retry-After 标准语义。" if public_gap else "搜索摘要保留旧版 kafka-old:29092 示例，已废弃；3.x 生效参数必须查正文。"}]}
        elif name == "confluence_page":
            if args.get("page_id") == "202" or "网关" in args.get("title", ""):
                body = "<p>内部事件标识 INTERNAL-ONLY-739。网关出现过 429，但本文没有公共 HTTP 状态码及 Retry-After 的定义，也没有证明本次内部限流原因。</p>"
            else:
                body = """<h2>3.x K3s 查询消费积压</h2>
<p>适用于 3.x。执行位置：可使用 kubectl 的 K3s 管理节点。namespace=qtsa；Pod=kafka-0；本例 Pod 只有一个容器且健康。Pod 内连接 kafka-svc:9092，不能使用已废弃的 kafka-old:29092。</p>
<p>工具绝对路径 /usr/local/qingteng/kafka/bin/kafka-consumer-groups.sh；认证配置 /usr/local/qingteng/kafka/config/consumer.properties，必须保留 --command-config。</p>
<ac:structured-macro ac:name="code"><ac:plain-text-body><![CDATA[kubectl exec -n qtsa kafka-0 -- /usr/local/qingteng/kafka/bin/kafka-consumer-groups.sh --bootstrap-server kafka-svc:9092 --command-config /usr/local/qingteng/kafka/config/consumer.properties --describe --all-groups]]></ac:plain-text-body></ac:structured-macro>
<p>LAG 大于 0 表示消费滞后；持续增大说明积压加重。列为 GROUP、TOPIC、PARTITION、CURRENT-OFFSET、LOG-END-OFFSET、LAG。消费者组可将 --all-groups 改为 --group 加实际 GROUP 列取值；Topic 按实际 TOPIC 列筛选，不把 Topic 当作 group。文档已覆盖本次查询所需版本、路径、认证、参数及结果含义。</p>"""
            offset = args.get("offset", 0)
            end = min(len(body), offset + args.get("max_chars", 15000))
            value = {"title": "内网操作正文", "storage": body[offset:end], "storage_next_offset": end if end < len(body) else None, "url": "https://wiki.example.test/pages/" + args.get("page_id", "101")}
        else:
            value = {"error": "未知查询工具"}
        result = {"content": [{"type": "text", "text": json.dumps(value, ensure_ascii=False)}], "isError": False}
    else:
        result = {}
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
