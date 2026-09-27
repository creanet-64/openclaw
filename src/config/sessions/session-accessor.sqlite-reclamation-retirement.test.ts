import { Worker } from "node:worker_threads";
import { expect, test, vi } from "vitest";
import { createOpenClawAgentDatabaseClaim } from "../../state/openclaw-agent-db-identity.js";
import { openOpenClawAgentDatabase } from "../../state/openclaw-agent-db.js";
import { withOpenClawTestState } from "../../test-utils/openclaw-test-state.js";
import * as sqliteArchive from "./session-accessor.sqlite-archive.js";
import { SqliteReclamationWorker } from "./session-accessor.sqlite-reclamation-worker.js";
import * as coordination from "./session-accessor.sqlite-worker-coordination.js";

test("retains an unused worker until native termination is confirmed", async () => {
  await withOpenClawTestState({ scenario: "minimal" }, async (state) => {
    const options = { agentId: "main", env: state.env };
    const database = openOpenClawAgentDatabase(options);
    const databaseOptions = { ...options, path: database.path };
    const claim = createOpenClawAgentDatabaseClaim(database, () => {});
    const native = new Worker(
      `const { parentPort } = require("node:worker_threads");
       parentPort.on("message", () => {});`,
      { eval: true, execArgv: [] },
    );
    const spawn = vi
      .spyOn(sqliteArchive, "createSqliteTranscriptArchiveWorker")
      .mockReturnValueOnce(native);
    // Control the coordination boundary: both requests refuse before dispatch,
    // but termination has not completed. Absence of a request alone is insufficient.
    const admission = vi
      .spyOn(coordination, "withSqliteMutationWorkerCoordination")
      .mockRejectedValueOnce(new Error("request refused before dispatch"))
      .mockRejectedValueOnce(new Error("close admission remains blocked"));
    const owner = new SqliteReclamationWorker(databaseOptions, claim.identity);
    try {
      await expect(
        owner.use(() =>
          owner.runCanonicalValidation({
            databaseOptions,
            claim,
            maxRows: 1,
            maxBytes: 1024,
            initializeCanonicalValidation: false,
            commitGate: new SharedArrayBuffer(Int32Array.BYTES_PER_ELEMENT),
            onCommitRequest: () => [],
            withWriteAdmission: async () => {},
          }),
        ),
      ).rejects.toThrow("request refused before dispatch");
      expect(native.threadId).toBeGreaterThan(0);
      await expect(owner.close()).rejects.toThrow("close admission remains blocked");
      expect(native.threadId).toBeGreaterThan(0);
      await native.terminate();
      await expect(owner.close()).resolves.toBeUndefined();
      expect(admission).toHaveBeenCalledTimes(2);
    } finally {
      await native.terminate();
      await owner.close();
      claim.release();
      admission.mockRestore();
      spawn.mockRestore();
    }
  });
});

test("does not infer safe retirement from native exit without a lease receipt after dispatch", async () => {
  await withOpenClawTestState({ scenario: "minimal" }, async (state) => {
    const options = { agentId: "main", env: state.env };
    const database = openOpenClawAgentDatabase(options);
    const databaseOptions = { ...options, path: database.path };
    const claim = createOpenClawAgentDatabaseClaim(database, () => {});
    // The transport consumes a work request and exits before delivering a lease.
    // This is intentionally not evidence that an actual worker acquired no lease.
    const native = new Worker(
      `const { parentPort } = require("node:worker_threads");
       parentPort.once("message", () => process.exit(3));`,
      { eval: true, execArgv: [] },
    );
    let receiveOwnerMessage: ((message: unknown) => void) | undefined;
    const on = native.on.bind(native);
    const listen = vi.spyOn(native, "on").mockImplementation((event, listener) => {
      if (event === "message") {
        receiveOwnerMessage ??= listener;
      }
      return on(event, listener);
    });
    const spawn = vi
      .spyOn(sqliteArchive, "createSqliteTranscriptArchiveWorker")
      .mockReturnValueOnce(native);
    const owner = new SqliteReclamationWorker(databaseOptions, claim.identity);
    try {
      await expect(
        owner.use(() =>
          owner.runCanonicalValidation({
            databaseOptions,
            claim,
            maxRows: 1,
            maxBytes: 1024,
            initializeCanonicalValidation: false,
            commitGate: new SharedArrayBuffer(Int32Array.BYTES_PER_ELEMENT),
            onCommitRequest: () => [],
            withWriteAdmission: async () => {},
          }),
        ),
      ).rejects.toThrow();
      expect(native.threadId).toBe(-1);
      await expect(owner.close()).rejects.toThrow("cleanup is uncertain");
    } finally {
      await native.terminate();
      // Supply a close receipt only for fixture teardown after the refusal proof.
      // Worker disposal removes its listeners, so call the captured owner receiver.
      receiveOwnerMessage?.({ type: "closed", settled: true, cleanupWarnings: [] });
      await owner.close();
      claim.release();
      listen.mockRestore();
      spawn.mockRestore();
    }
  });
});
