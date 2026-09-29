import json
import sys


def reply(request, result):
    response = {"jsonrpc": "2.0", "id": request["id"], "result": result}
    print(json.dumps(response, separators=(",", ":")), flush=True)


for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    method = request.get("method")
    if method == "initialize":
        reply(
            request,
            {
                "protocolVersion": request.get("params", {}).get(
                    "protocolVersion", "2025-06-18"
                ),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "yasm-ping", "version": "1.0.0"},
            },
        )
    elif method == "tools/list":
        reply(
            request,
            {
                "tools": [
                    {
                        "name": "ping",
                        "description": "Return a deterministic pong response",
                        "inputSchema": {"type": "object", "properties": {}},
                    }
                ]
            },
        )
    elif method == "tools/call" and request.get("params", {}).get("name") == "ping":
        reply(
            request,
            {"content": [{"type": "text", "text": "pong-yasm"}], "isError": False},
        )
    else:
        response = {
            "jsonrpc": "2.0",
            "id": request["id"],
            "error": {"code": -32601, "message": "Method not found"},
        }
        print(json.dumps(response, separators=(",", ":")), flush=True)
