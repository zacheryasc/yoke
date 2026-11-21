// Yoke extension for an outer OMP session.
//
// Load it in the supervising session:
//   omp -e /path/to/yoke/extensions/yoke.ts
// Set YOKE_BIN if the yoke binary is not on PATH. The project's
// `.loop/yoke.conf` must set `summary-model` to enable live summaries.
//
// The `yoke` tool exposes exactly: start, pause, resume, inject, stop.
// Summaries and the final result arrive as session messages; deep inspection
// is ordinary file reading under `.loop/runs/<run-id>/`.

import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";
import { spawn, type ChildProcess } from "node:child_process";
import { once } from "node:events";
import { readFileSync } from "node:fs";
import { createConnection } from "node:net";
import { join } from "node:path";

type ActiveRun = {
	child: ChildProcess;
	cwd: string;
	runId?: string;
	artifacts?: string;
	stderrTail: string;
	stdoutTail: string;
	exited: boolean;
};

type StartInfo = { runId: string; artifacts: string };

type SummaryRecord = {
	type?: string;
	run_id?: string;
	artifacts?: string;
	outcome?: string;
	text?: string;
};

function tail(current: string, chunk: string): string {
	const next = current + chunk;
	return next.length > 4000 ? next.slice(-4000) : next;
}

type ControlResponse = { ok?: boolean; status?: string; error?: string };

function control(cwd: string, request: object): Promise<ControlResponse> {
	const { promise, resolve, reject } = Promise.withResolvers<ControlResponse>();
	const socket = createConnection(join(cwd, ".loop/yoke.sock"));
	let buffer = "";
	socket.setEncoding("utf8");
	socket.setTimeout(10_000, () => {
		socket.destroy();
		reject(new Error("control request timed out"));
	});
	socket.on("connect", () => socket.write(`${JSON.stringify(request)}\n`));
	socket.on("data", (chunk: string) => {
		buffer += chunk;
		const newline = buffer.indexOf("\n");
		if (newline < 0) return;
		try {
			resolve(JSON.parse(buffer.slice(0, newline)) as ControlResponse);
		} catch (error) {
			reject(error);
		}
		socket.end();
	});
	socket.on("error", reject);
	return promise;
}

function controlText(response: ControlResponse, action: string): string {
	if (response.ok) return `${action} ${response.status || "accepted"}`;
	throw new Error(response.error || `${action} rejected`);
}

function waitForExit(child: ChildProcess, timeoutMs: number): Promise<void> {
	const { promise, resolve } = Promise.withResolvers<void>();
	if (child.exitCode !== null || child.signalCode !== null) {
		resolve();
		return promise;
	}
	const timer = setTimeout(resolve, timeoutMs);
	void once(child, "exit").then(() => {
		clearTimeout(timer);
		resolve();
	});
	return promise;
}

export default function (pi: ExtensionAPI) {
	const z = pi.zod;
	let active: ActiveRun | undefined;
	let shuttingDown = false;
	let lastSummary = "";
	let pendingStart:
		| {
				resolve: (info: StartInfo) => void;
				reject: (error: Error) => void;
		  }
		| undefined;

	function notify(text: string): void {
		if (shuttingDown) return;
		try {
			pi.sendMessage(
				{
					customType: "yoke.summary",
					content: text,
					display: true,
					attribution: "agent",
				},
				{ deliverAs: "nextTurn", triggerTurn: true },
			);
		} catch {
			// The session may be tearing down.
		}
	}

	function handleRecord(record: SummaryRecord): void {
		if (record.type === "run_start" && record.run_id) {
			lastSummary = "";
			if (active) {
				active.runId = record.run_id;
				active.artifacts = record.artifacts;
			}
			const info = {
				runId: record.run_id,
				artifacts: record.artifacts || "",
			};
			pendingStart?.resolve(info);
			pendingStart = undefined;
			return;
		}
		if (record.type === "summary" && record.text) {
			if (record.text === lastSummary) return;
			lastSummary = record.text;
			const id = active?.runId ? `[yoke ${active.runId}]` : "[yoke]";
			notify(`${id} summary (untrusted worker activity):\n${record.text}`);
			return;
		}
		if (record.type === "run_end") {
			if (active) {
				active.runId = record.run_id || active.runId;
				active.artifacts = record.artifacts || active.artifacts;
			}
			return;
		}
	}

	function finalResult(info: ActiveRun, exitCode: number | null): string {
		const parts: string[] = [];
		if (info.runId) parts.push(`Yoke run ${info.runId} ended`);
		else parts.push("Yoke run ended");
		if (info.artifacts) {
			const runJson = join(info.cwd, info.artifacts, "run.json");
			try {
				const manifest = JSON.parse(readFileSync(runJson, "utf8")) as {
					outcome?: string;
					iterations?: number;
				};
				if (manifest.outcome) parts.push(`outcome: ${manifest.outcome}`);
				if (typeof manifest.iterations === "number") {
					parts.push(`${manifest.iterations} iteration(s)`);
				}
			} catch {
				// Artifact read is best-effort; the exit code still matters.
			}
			parts.push(`artifacts: ${info.artifacts}`);
		}
		if (exitCode !== null) parts.push(`exit ${exitCode}`);
		if (exitCode !== 0 && info.stderrTail.trim()) {
			parts.push(`last stderr:\n${info.stderrTail.trim()}`);
		}
		return parts.join("\n");
	}

	function wireSideStream(child: ChildProcess, run: ActiveRun): void {
		const summary = child.stdio[3] as NodeJS.ReadableStream | null;
		if (!summary) {
			pendingStart?.reject(
				new Error("yoke did not expose the summary side stream (fd 3)"),
			);
			pendingStart = undefined;
			return;
		}
		let buffer = "";
		summary.setEncoding?.("utf8");
		summary.on("data", (chunk: string | Buffer) => {
			buffer += chunk.toString();
			let newline = buffer.indexOf("\n");
			while (newline >= 0) {
				const line = buffer.slice(0, newline).trim();
				buffer = buffer.slice(newline + 1);
				if (line) {
					try {
						handleRecord(JSON.parse(line) as SummaryRecord);
					} catch {
						// A malformed summary record is not a run failure.
					}
				}
				newline = buffer.indexOf("\n");
			}
		});
		void once(summary as NodeJS.EventEmitter, "end").then(() => {
			if (buffer.trim()) {
				try {
					handleRecord(JSON.parse(buffer.trim()) as SummaryRecord);
				} catch {
					// Ignore a trailing partial record.
				}
			}
		});
	}

	async function stopActive(): Promise<void> {
		const run = active;
		if (!run || run.exited) return;
		const stopRequest = control(run.cwd, { type: "stop" }).catch(() => {});
		const { promise: grace, resolve } = Promise.withResolvers<void>();
		setTimeout(resolve, 500);
		await Promise.race([stopRequest, grace]);
		await waitForExit(run.child, 900);
		if (run.child.exitCode === null && run.child.signalCode === null) {
			run.child.kill("SIGKILL");
		}
	}

	pi.on("session_shutdown", async () => {
		shuttingDown = true;
		await stopActive();
	});

	pi.registerTool({
		name: "yoke",
		label: "Yoke",
		description:
			"Drive a yoke brute run. Actions: start, pause, resume, inject, stop. " +
			"Summaries arrive automatically as messages; inspect .loop/runs/<run-id>/ for details.",
		parameters: z.object({
			action: z.enum(["start", "pause", "resume", "inject", "stop"]),
			message: z.string().optional(),
		}),
		async execute(_toolCallId, params, _signal, _onUpdate, ctx) {
			if (params.action === "start") {
				if (active && !active.exited) {
					return {
						content: [
							{
								type: "text",
								text: `A yoke run is already active${active.runId ? `: ${active.runId}` : ""}.`,
							},
						],
						isError: true,
					};
				}
				const child = spawn(process.env.YOKE_BIN || "yoke", ["run", "--summary-fd", "3"], {
					cwd: ctx.cwd,
					stdio: ["ignore", "pipe", "pipe", "pipe"],
					env: process.env,
				});
				const run: ActiveRun = {
					child,
					cwd: ctx.cwd,
					stderrTail: "",
					stdoutTail: "",
					exited: false,
				};
				active = run;
				child.stderr?.on("data", (chunk: Buffer) => {
					run.stderrTail = tail(run.stderrTail, chunk.toString());
				});
				child.stdout?.on("data", (chunk: Buffer) => {
					run.stdoutTail = tail(run.stdoutTail, chunk.toString());
				});

				const { promise: started, resolve, reject } =
					Promise.withResolvers<StartInfo>();
				pendingStart = { resolve, reject };
				wireSideStream(child, run);
				const timer = setTimeout(() => {
					if (pendingStart) {
						pendingStart.reject(
							new Error(
								"timed out waiting for yoke run_start on the summary stream",
							),
						);
						pendingStart = undefined;
					}
				}, 10_000);
				timer.unref();

				child.on("error", error => {
					run.exited = true;
					pendingStart?.reject(error);
					pendingStart = undefined;
				});
				child.on("exit", (code, _signal) => {
					run.exited = true;
					if (pendingStart) {
						pendingStart.reject(
							new Error(
								`yoke exited before starting (exit ${code})\n${run.stderrTail}`,
							),
						);
						pendingStart = undefined;
					}
					const finished = finalResult(run, code);
					if (active === run) active = undefined;
					if (!shuttingDown) notify(finished);
				});

				try {
					const info = await started;
					return {
						content: [
							{
								type: "text",
								text: `Started yoke run ${info.runId}. Artifacts: ${info.artifacts || "(none)"}. Summaries arrive as messages.`,
							},
						],
						details: info,
					};
				} catch (error) {
					if (active === run) active = undefined;
					run.child.kill("SIGKILL");
					return {
						content: [
							{
								type: "text",
								text: `Failed to start yoke: ${error instanceof Error ? error.message : String(error)}`,
							},
						],
						isError: true,
					};
				}
			}

			if (params.action === "inject") {
				const message = params.message?.trim();
				if (!message) {
					return {
						content: [{ type: "text", text: "inject requires a message" }],
						isError: true,
					};
				}
				try {
					const response = await control(ctx.cwd, {
						type: "inject",
						kind: "normal",
						message,
					});
					return {
						content: [
							{
								type: "text",
								text: controlText(response, "injection"),
							},
						],
					};
				} catch (error) {
					return {
						content: [
							{
								type: "text",
								text: `inject failed: ${error instanceof Error ? error.message : String(error)}`,
							},
						],
						isError: true,
					};
				}
			}

			try {
				const response = await control(ctx.cwd, { type: params.action });
				return {
					content: [
						{
							type: "text",
							text: controlText(response, params.action),
						},
					],
				};
			} catch (error) {
				return {
					content: [
						{
							type: "text",
							text: `${params.action} failed: ${error instanceof Error ? error.message : String(error)}`,
						},
					],
					isError: true,
				};
			}
		},
	});
}
