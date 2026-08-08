import crypto from "node:crypto";
import fs from "node:fs";
import http from "node:http";
import path from "node:path";

const [metricsArgument, readyArgument] = process.argv.slice(2);

if (!metricsArgument || !readyArgument) {
  throw new Error("必须提供指标文件和就绪文件路径");
}

const metricsFile = path.resolve(metricsArgument);
const readyFile = path.resolve(readyArgument);
fs.mkdirSync(path.dirname(metricsFile), { recursive: true });
fs.mkdirSync(path.dirname(readyFile), { recursive: true });
const metricsDescriptor = fs.openSync(metricsFile, "w");

let requestIndex = 0;
let closing = false;

function sha256(value) {
  return crypto.createHash("sha256").update(value).digest("hex");
}

function parseTaskBlock(userMessage) {
  const fenced = /^```json\r?\n([\s\S]*)\r?\n```$/u.exec(userMessage.trim());
  if (!fenced) {
    throw new Error("user message 不是唯一且闭合的 json Markdown 围栏");
  }

  const document = JSON.parse(fenced[1]);
  if (!document || typeof document !== "object" || Array.isArray(document)) {
    throw new Error("user message 根必须是 JSON object");
  }
  if (!Array.isArray(document.groups)) {
    throw new Error("user message 缺少 groups 数组");
  }

  const activeUnits = [];
  const seenIds = new Set();
  for (const group of document.groups) {
    if (!group || typeof group !== "object" || !Array.isArray(group.units)) {
      throw new Error("group 缺少 units 数组");
    }
    for (const unit of group.units) {
      if (!unit || typeof unit !== "object" || unit.id === undefined) {
        continue;
      }
      if (typeof unit.id !== "string" || !/^(?:0|[1-9][0-9]*)$/u.test(unit.id)) {
        throw new Error("active unit 使用了非法临时 ID");
      }
      if (seenIds.has(unit.id)) {
        throw new Error("active unit 临时 ID 重复");
      }
      if (unit.type !== "strict" && unit.type !== "free") {
        throw new Error("active unit 缺少有效 type");
      }
      if (!Array.isArray(unit.text) || !unit.text.every((line) => typeof line === "string")) {
        throw new Error("active unit text 必须是字符串数组");
      }
      seenIds.add(unit.id);
      activeUnits.push({ id: unit.id, text: unit.text });
    }
  }
  if (activeUnits.length === 0) {
    throw new Error("user message 没有 active unit");
  }
  return activeUnits;
}

function translateNaturalText(value) {
  if (/^\s*$/u.test(value)) {
    return value;
  }
  const leading = /^\s*/u.exec(value)[0];
  const trailing = /\s*$/u.exec(value)[0];
  return `${leading}基准译文${sha256(value).slice(0, 12)}${trailing}`;
}

function translateLine(line) {
  if (line.length === 0) {
    return "";
  }
  return line
    .split(/(⟦ATT_[^⟧]+⟧)/gu)
    .map((part) => (part.startsWith("⟦ATT_") ? part : translateNaturalText(part)))
    .join("");
}

function buildAssistantContent(activeUnits) {
  const translations = {};
  for (const unit of activeUnits) {
    translations[unit.id] = unit.text.map(translateLine);
  }
  return JSON.stringify({
    think: "本地性能 Provider 已按当前 TaskBlock 生成确定性译文。",
    translations,
  });
}

function respondJson(response, statusCode, value) {
  const payload = JSON.stringify(value);
  const bytes = Buffer.byteLength(payload);
  response.writeHead(statusCode, {
    "content-type": "application/json",
    "content-length": bytes,
    connection: "keep-alive",
  });
  response.end(payload);
  return bytes;
}

function recordMetric(value) {
  fs.writeSync(metricsDescriptor, `${JSON.stringify(value)}\n`, null, "utf8");
}

function closeServer() {
  if (closing) {
    return;
  }
  closing = true;
  server.close(() => {
    fs.closeSync(metricsDescriptor);
  });
  server.closeIdleConnections?.();
  setTimeout(() => server.closeAllConnections?.(), 5_000).unref();
}

const server = http.createServer((request, response) => {
  if (request.method === "GET" && request.url === "/__att_performance__/health") {
    respondJson(response, 200, { ready: true, requests: requestIndex });
    return;
  }
  if (request.method === "POST" && request.url === "/__att_performance__/shutdown") {
    respondJson(response, 200, { closing: true });
    setImmediate(closeServer);
    return;
  }
  if (request.method !== "POST" || request.url !== "/v1/chat/completions") {
    respondJson(response, 404, { error: "not found" });
    return;
  }

  const index = requestIndex;
  requestIndex += 1;
  const receivedAt = new Date().toISOString();
  const startedAt = performance.now();
  const chunks = [];

  request.on("data", (chunk) => chunks.push(chunk));
  request.on("end", () => {
    let requestBytes = 0;
    let responseBytes = 0;
    let status = 200;
    let scenario = null;
    let unitCount = 0;
    let userMessageHash = null;
    let systemMessageHash = null;
    let assistantContentHash = null;

    try {
      const rawBody = Buffer.concat(chunks);
      requestBytes = rawBody.length;
      const body = JSON.parse(rawBody.toString("utf8"));
      const authorization = request.headers.authorization;
      const authorizationMatch = typeof authorization === "string"
        ? /^Bearer att-performance-([a-z0-9-]+)$/u.exec(authorization)
        : null;
      scenario = authorizationMatch?.[1] ?? null;
      if (scenario === null) {
        throw new Error("请求缺少有效的性能测试 Bearer 标识");
      }
      if (!Array.isArray(body.messages)) {
        throw new Error("请求缺少 messages 数组");
      }

      const userMessage = [...body.messages]
        .reverse()
        .find((message) => message?.role === "user");
      const systemMessage = body.messages.find((message) => message?.role === "system");
      if (!userMessage || typeof userMessage.content !== "string") {
        throw new Error("请求缺少 user message");
      }

      userMessageHash = sha256(userMessage.content);
      if (systemMessage && typeof systemMessage.content === "string") {
        systemMessageHash = sha256(systemMessage.content);
      }

      const activeUnits = parseTaskBlock(userMessage.content);
      unitCount = activeUnits.length;
      const assistantContent = buildAssistantContent(activeUnits);
      assistantContentHash = sha256(assistantContent);
      responseBytes = respondJson(response, 200, {
        id: `att-performance-${index}`,
        choices: [
          {
            index: 0,
            message: { role: "assistant", content: assistantContent },
            finish_reason: "stop",
          },
        ],
        usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
      });
    } catch (error) {
      status = 500;
      responseBytes = respondJson(response, status, {
        error: error instanceof Error ? error.message : "unknown provider error",
      });
    } finally {
      recordMetric({
        request_index: index,
        scenario,
        received_at: receivedAt,
        completed_at: new Date().toISOString(),
        duration_ms: performance.now() - startedAt,
        request_bytes: requestBytes,
        response_bytes: responseBytes,
        unit_count: unitCount,
        user_message_sha256: userMessageHash,
        system_message_sha256: systemMessageHash,
        assistant_content_sha256: assistantContentHash,
        status,
      });
    }
  });
});

server.on("clientError", (error, socket) => {
  socket.end("HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n");
  recordMetric({
    request_index: null,
    scenario: null,
    received_at: new Date().toISOString(),
    completed_at: new Date().toISOString(),
    duration_ms: 0,
    request_bytes: 0,
    response_bytes: 0,
    unit_count: 0,
    user_message_sha256: null,
    system_message_sha256: null,
    assistant_content_sha256: null,
    status: 400,
    transport_error: error.message,
  });
});

server.listen(0, "127.0.0.1", () => {
  const address = server.address();
  if (!address || typeof address === "string") {
    throw new Error("无法取得本地 Provider 端口");
  }
  fs.writeFileSync(
    readyFile,
    JSON.stringify({ port: address.port, pid: process.pid, ready_at: new Date().toISOString() }),
    "utf8",
  );
});

for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, closeServer);
}
