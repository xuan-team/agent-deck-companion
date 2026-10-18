// Host half of the Agent Deck bridge.
//
// The desktop companion used to drive the DSH tab through AppleScript, which
// dies whenever macOS drops the "control Google Chrome" grant. Everything here
// goes over the DSH server's own authenticated `/api` channel instead:
//
//   POST /api/deck/select  <- the companion asks for a session
//   GET  /api/deck/stream  -> server-sent events to every open tab
//   POST /api/deck/ack     <- the tab confirms the session open ran
//   GET  /api/deck/tabs    -> how many tabs are connected, and as which browser
//
// `select` waits for the first ack before responding, so one synchronous call
// tells the companion whether a tab really switched. The tab count is what lets
// the companion raise the existing page instead of opening a duplicate tab;
// without it the only "is a tab open?" probe left is AppleScript, which macOS
// can revoke at any time. Session ids are only ever matched against SESSION_ID
// and forwarded; nothing here evaluates them.
//
// The other direction is a push, so the companion does not have to poll the
// projection files every five seconds:
//
//   POST 127.0.0.1:<companion>/api/dsh/push  <- "the session set changed"
//
// It carries a reason and the affected session ids, never authoritative state:
// the companion re-samples DSH with its own reader, so DSH semantics stay in
// one place and a dropped push costs latency, not correctness.
import { readFile } from "node:fs/promises";
import { homedir } from "node:os";
import { join } from "node:path";

/** Cordis plugin name used by loader diagnostics. */
const name = "deck-bridge";
/** The host Connection service, which owns the authenticated `/api` channel. */
const inject = ["connection"];

/** Mirrors the companion's session id rule, plus a bounded length. */
const SESSION_ID = /^session-[A-Za-z0-9_-]{1,72}$/;
/** Lowercase hex, matching the nonce the companion generates. */
const NONCE = /^[0-9a-f]{1,32}$/;
/** How long a select may wait for a tab to acknowledge before giving up. */
const ACK_TIMEOUT_MS = 5000;
/** Comment frame interval, well under any proxy idle timeout. */
const KEEPALIVE_MS = 20000;
/** A browser holds one ordinary HTTP poll at a time, renewed before this age. */
const POLL_TIMEOUT_MS = 20000;
const POLL_MAX_AGE_MS = 30000;
const CLIENT_ID = /^[0-9a-f]{32}$/;

/** Companion loopback API; overridable for an isolated QA run. */
const COMPANION_PORT =
	Number(process.env.AGENT_DECK_COMPANION_PORT) > 0
		? Number(process.env.AGENT_DECK_COMPANION_PORT)
		: 34115;
const COMPANION_PUSH = `http://127.0.0.1:${COMPANION_PORT}/api/dsh/push`;
/** The same secret the hook bridge uses, written 0600 by the companion. */
const TOKEN_PATH = join(
	process.env.AGENT_DECK_COMPANION_HOME || join(homedir(), ".agent-deck-companion"),
	"hook-token"
);
/** Companion must never make the DSH host wait on it. */
const PUSH_TIMEOUT_MS = 800;
/** A tool-heavy turn emits several events; collapse them into one push. */
const COALESCE_MS = 400;
/** At most one failure line per window, so a closed companion stays quiet. */
const FAILURE_LOG_MS = 30000;

function respond(body, status = 200) {
	return new Response(JSON.stringify(body), {
		status,
		headers: { "content-type": "application/json" }
	});
}

async function readJson(request) {
	try {
		return await request.json();
	} catch {
		return void 0;
	}
}

function sessionFrame(sessionId, nonce) {
	return `event: select\ndata: ${JSON.stringify({ sessionId, nonce })}\n\n`;
}

/**
* Name the browser behind a request so the companion can raise the right app.
* Chrome's own UA advertises Safari too, so Chromium engines are matched first.
* @param request - an incoming `/api/deck/stream` request.
* @returns "chrome", "safari" or "other".
*/
function browserOf(request) {
	const userAgent = request.headers.get("user-agent") ?? "";
	if (/Chrome|Chromium|Edg\//i.test(userAgent)) return "chrome";
	if (/Safari/i.test(userAgent)) return "safari";
	return "other";
}

function apply(ctx) {
	/** One entry per connected tab, holding everything that dies with it. */
	const clients = new Map();
	/** Browser tabs using ordinary long-poll requests instead of SSE. */
	const polls = new Map();
	/** Nonce -> settle, filled by the first ack or the timeout. */
	const pending = new Map();

	/** Coalescing state for the companion push. */
	let pushTimer = null;
	let pushReason = "unspecified";
	let pushSessionId = null;
	let pushConnected = false;
	let lastFailureLogAt = 0;

	const logPushFailure = (message) => {
		const now = Date.now();
		if (now - lastFailureLogAt < FAILURE_LOG_MS) return;
		lastFailureLogAt = now;
		console.error(`dsh-deck-bridge push: ${message}`);
	};

	/**
	 * Fire-and-forget: read the companion's hook token, POST the trigger, drop
	 * every failure. The companion keeps its own poll as a fallback, so losing
	 * a push costs latency, never state.
	 */
	const sendPush = async () => {
		let token;
		try {
			token = (await readFile(TOKEN_PATH, "utf8")).trim();
		} catch {
			// The companion never installed its hook bridge; nothing to notify.
			return;
		}
		if (token === "") return;
		const request = {
			method: "POST",
			headers: {
				"content-type": "application/json",
				"x-agent-deck-token": token
			},
			body: JSON.stringify({
				reason: pushReason,
				sessions: pushSessionId === null ? [] : [pushSessionId]
			})
		};
		if (typeof AbortSignal.timeout === "function") {
			request.signal = AbortSignal.timeout(PUSH_TIMEOUT_MS);
		}
		try {
			const response = await fetch(COMPANION_PUSH, request);
			if (!response.ok) {
				logPushFailure(`HTTP ${response.status}`);
				return;
			}
			if (!pushConnected) {
				pushConnected = true;
				console.log(`dsh-deck-bridge push: connected to companion at ${COMPANION_PUSH}`);
			}
		} catch (error) {
			logPushFailure(error?.message ?? String(error));
		}
	};

	/** Collapse a burst of session events into one push. */
	const schedulePush = (reason, sessionId) => {
		pushReason = reason;
		if (typeof sessionId === "string" && SESSION_ID.test(sessionId)) {
			pushSessionId = sessionId;
		}
		if (pushTimer !== null) return;
		pushTimer = setTimeout(() => {
			pushTimer = null;
			void sendPush();
		}, COALESCE_MS);
		pushTimer.unref?.();
	};

	// DSH's API-level session feed: the same events the browser sidebar
	// renders, which is exactly the granularity a "look now" trigger wants.
	// `agent/status` and `session/event` fire per turn and per token, are far
	// chattier, and cannot say anything the projection file does not already
	// hold by the time the companion re-samples it.
	ctx.on("api-session/added", (summary) => schedulePush("added", summary?.sessionId));
	ctx.on("api-session/removed", (sessionId) => schedulePush("removed", sessionId));
	ctx.on("api-session/status", (sessionId) => schedulePush("status", sessionId));
	ctx.on("api-session/activity", (sessionId) => schedulePush("activity", sessionId));
	ctx.on("api-session/error", (sessionId) => schedulePush("error", sessionId));

	const prunePolls = () => {
		const now = Date.now();
		for (const [id, client] of polls) {
			if (now - client.seenAt > POLL_MAX_AGE_MS) {
				client.deliver?.(null);
				polls.delete(id);
			}
		}
	};

	const connectedBrowsers = () => {
		prunePolls();
		return [...clients.values(), ...polls.values()];
	};

	const forget = (controller) => {
		const client = clients.get(controller);
		if (client === undefined) return;
		clearInterval(client.keepalive);
		client.signal.removeEventListener("abort", client.onAbort);
		clients.delete(controller);
	};

	const push = (frame) => {
		for (const controller of Array.from(clients.keys())) {
			try {
				controller.enqueue(frame);
			} catch {
				forget(controller);
			}
		}
	};

	const waitForAck = (nonce, targets) => new Promise((resolve) => {
		// A repeat of the same nonce must not orphan the earlier waiter, whose
		// timer would otherwise delete the entry this promise is waiting on.
		pending.get(nonce)?.resolve("superseded");
		let entry;
		const timer = setTimeout(() => {
			pending.delete(nonce);
			resolve({ outcome: entry.rejected > 0 ? "client-rejected" : "ack-timeout", rejected: entry.rejected, detail: entry.detail });
		}, ACK_TIMEOUT_MS);
		entry = {
			targets,
			rejected: 0,
			detail: void 0,
			resolve: (result) => {
				clearTimeout(timer);
				pending.delete(nonce);
				resolve({ outcome: result, rejected: entry.rejected, detail: entry.detail });
			}
		};
		pending.set(nonce, entry);
	});

	ctx.connection.fetch.register({
		path: "/api/deck/select",
		methods: ["POST"],
		requestBody: "buffered",
		fetch: async (request) => {
			const body = await readJson(request);
			const sessionId = body?.sessionId;
			const nonce = body?.nonce;
			if (typeof sessionId !== "string" || !SESSION_ID.test(sessionId)) {
				return respond({ ok: false, acked: false, reason: "bad-session-id" }, 400);
			}
			if (typeof nonce !== "string" || !NONCE.test(nonce)) {
				return respond({ ok: false, acked: false, reason: "bad-nonce" }, 400);
			}
			const tabs = connectedBrowsers().length;
			if (tabs === 0) return respond({ ok: false, tabs: 0, acked: false, reason: "no-client" });
			// Register the acknowledgement before delivering to any browser tab.
			const result = waitForAck(nonce, tabs);
			for (const client of polls.values()) {
				if (client.deliver) client.deliver({ sessionId, nonce });
				else client.queued = { sessionId, nonce };
			}
			push(sessionFrame(sessionId, nonce));
			const { outcome, rejected, detail } = await result;
			const acked = outcome === "acked";
			return respond({ ok: acked, tabs, rejected, acked, reason: acked ? void 0 : outcome, detail });
		}
	});

	// Ordinary authenticated GET requests are less fragile than a long-lived
	// browser EventSource. Each tab immediately renews its poll after a response.
	ctx.connection.fetch.register({
		path: "/api/deck/next",
		methods: ["GET"],
		requestBody: "buffered",
		fetch: (request) => {
			const id = new URL(request.url).searchParams.get("clientId");
			if (id === null || !CLIENT_ID.test(id)) return respond({ ok: false, reason: "bad-client-id" }, 400);
			const previous = polls.get(id);
			previous?.deliver?.(null);
			const client = { browser: browserOf(request), seenAt: Date.now(), queued: previous?.queued, deliver: null };
			polls.set(id, client);
			if (client.queued) {
				const command = client.queued;
				client.queued = null;
				return respond({ ok: true, command });
			}
			return new Promise((resolve) => {
				const finish = (command) => {
					if (client.deliver !== finish) return;
					client.deliver = null;
					clearTimeout(timer);
					request.signal.removeEventListener("abort", onAbort);
					resolve(respond({ ok: true, command }));
				};
				const onAbort = () => {
					finish(null);
					if (polls.get(id) === client) polls.delete(id);
				};
				const timer = setTimeout(() => finish(null), POLL_TIMEOUT_MS);
				client.deliver = finish;
				request.signal.addEventListener("abort", onAbort, { once: true });
				if (request.signal.aborted) onAbort();
			});
		}
	});

	ctx.connection.fetch.register({
		path: "/api/deck/stream",
		methods: ["GET"],
		requestBody: "buffered",
		fetch: (request) => {
			const browser = browserOf(request);
			let controller;
			const readable = new ReadableStream({
				start(sent) {
					controller = sent;
					const onAbort = () => {
						forget(sent);
						try { sent.close(); } catch {}
					};
					const keepalive = setInterval(() => {
						try {
							sent.enqueue(": keepalive\n\n");
						} catch {
							forget(sent);
						}
					}, KEEPALIVE_MS);
					clients.set(sent, { keepalive, browser, signal: request.signal, onAbort });
					request.signal.addEventListener("abort", onAbort, { once: true });
					if (request.signal.aborted) return onAbort();
					sent.enqueue(": deck-bridge\n\n");
				},
				cancel() {
					forget(controller);
				}
			});
			return new Response(readable, {
				headers: {
					"content-type": "text/event-stream",
					"cache-control": "no-cache",
					"x-accel-buffering": "no"
				}
			});
		}
	});

	ctx.connection.fetch.register({
		path: "/api/deck/tabs",
		methods: ["GET"],
		requestBody: "buffered",
		fetch: () => {
			const connected = connectedBrowsers();
			const browsers = [];
			for (const client of connected) {
				if (!browsers.includes(client.browser)) browsers.push(client.browser);
			}
			return respond({ ok: true, tabs: connected.length, polls: polls.size, streams: clients.size, browsers });
		}
	});

	ctx.connection.fetch.register({
		path: "/api/deck/ack",
		methods: ["POST"],
		requestBody: "buffered",
		fetch: async (request) => {
			const body = await readJson(request);
			const nonce = body?.nonce;
			if (typeof nonce !== "string" || !NONCE.test(nonce)) {
				return respond({ ok: false, reason: "bad-nonce" }, 400);
			}
			const waiter = pending.get(nonce);
			if (waiter === void 0) return respond({ ok: false, reason: "unknown-nonce" });
			waiter.resolve("acked");
			return respond({ ok: true });
		}
	});

	ctx.connection.fetch.register({
		path: "/api/deck/nack",
		methods: ["POST"],
		requestBody: "buffered",
		fetch: async (request) => {
			const body = await readJson(request);
			const nonce = body?.nonce;
			if (typeof nonce !== "string" || !NONCE.test(nonce)) return respond({ ok: false, reason: "bad-nonce" }, 400);
			const waiter = pending.get(nonce);
			if (waiter === void 0) return respond({ ok: false, reason: "unknown-nonce" });
			if (typeof body.error === "string") {
				waiter.detail = body.error.slice(0, 160).replace(/[\r\n]/g, " ");
			}
			waiter.rejected += 1;
			if (waiter.rejected >= waiter.targets) waiter.resolve("client-rejected");
			return respond({ ok: true });
		}
	});

	// A companion call must not hang on a teardown that leaves it unresolved.
	ctx.effect(() => () => {
		if (pushTimer !== null) {
			clearTimeout(pushTimer);
			pushTimer = null;
		}
		for (const waiter of Array.from(pending.values())) waiter.resolve("plugin-stopped");
		for (const [id, client] of polls) {
			client.deliver?.(null);
			polls.delete(id);
		}
		for (const sent of Array.from(clients.keys())) {
			forget(sent);
			try {
				sent.close();
			} catch {}
		}
	});
}

export { apply, inject, name };
