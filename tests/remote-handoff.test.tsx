import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import i18n from "i18next";
import "@/i18n";
import { api } from "@/lib/api";
import { RemoteHandoff } from "@/components/RemoteHandoff";
import { HandoffNotifications } from "@/components/HandoffNotifications";
import { handoffDraft, handoffState, handoffSummary, type RemoteHandoffRecord, type RemoteHandoffThread } from "@/lib/remoteHandoff";
import { assertResourceParity } from "@/i18n/resources";

const store = vi.hoisted(() => ({ records: [] as RemoteHandoffRecord[] }));
vi.mock("@/lib/handoffStore", () => ({ useHandoffRecords: () => store.records, refreshHandoffs: vi.fn().mockResolvedValue(undefined) }));
const thread: RemoteHandoffThread = { threadId: "local", name: "Finish word-count CLI", repo: "E:/demo", branch: "main", updatedAt: 1, model: "model", modelProvider: "vellum", effort: "high", active: false, handedOff: false };
function record(state = "completed"): RemoteHandoffRecord {
  return { id: "handoff", hostId: "host", source: thread, snapshot: { base: "a".repeat(40), commit: "b".repeat(40), localRef: "refs/vellum-handoff/handoff", changes: [] }, request: {}, stage: "running", error: null,
    remote: { handoffId: "handoff", phase: "running", state, threadId: "remote-thread", turnId: "turn", turnStatus: state, plan: ["Implement", "Test"], recentOutput: "tests passed", summary: "Ready for review", capture: null },
    result: { captureOperation: "capture", capture: { resultRef: "refs/vellum-handoff-results/handoff/capture", commit: "c".repeat(40), snapshotCommit: "b".repeat(40), partial: false, diffstat: [{ path: "wordcount.py", added: 46, deleted: 1 }] }, fetched: true, outcome: null, branch: null, applyIntent: false, cleaned: false, acknowledged: false } };
}
beforeEach(async () => {
  store.records = [];
  await i18n.changeLanguage("en");
  vi.spyOn(api, "listRemoteHandoffThreads").mockResolvedValue([thread]);
  vi.spyOn(api, "preflightRemoteHandoff").mockResolvedValue({ thread, changes: [{ path: "wordcount.py", staged: true, untracked: false }], blockers: [], warnings: [], remoteOs: "linux", localRefNotice: "ref notice" });
  vi.spyOn(api, "reviewRemoteHandoff").mockResolvedValue({ canApply: true, patch: "+ remote result" });
  vi.spyOn(api, "getRemoteHandoffThreadLink").mockResolvedValue("codex://threads/remote-thread?hostId=remote-ssh-discovered%3Ajetson-orin");
  vi.spyOn(api, "openRemoteHandoffThread").mockResolvedValue(undefined);
});
afterEach(() => { cleanup(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });
const view = () => render(<RemoteHandoff hostId="host" hostName="Jetson" active />);
describe("remote handoff", () => {
  it("opens through Rust by handoff ID and keeps thread-ID copying secondary", async () => {
    store.records = [record()];
    const copy = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal("navigator", { clipboard: { writeText: copy } });
    view();
    fireEvent.click(await screen.findByRole("button", { name: "Open in Codex App" }));
    await waitFor(() => expect(api.openRemoteHandoffThread).toHaveBeenCalledWith("handoff"));
    fireEvent.click(screen.getByRole("button", { name: "Copy remote thread ID" }));
    await waitFor(() => expect(copy).toHaveBeenCalledWith("remote-thread"));
  });
  it("offers only ID copying and a host setup hint without a Desktop host ID", async () => {
    store.records = [record()];
    vi.mocked(api.getRemoteHandoffThreadLink).mockResolvedValue(null);
    view();
    expect(await screen.findByText(i18n.t("handoff.addHostToCodex"))).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Open in Codex App" })).toBeNull();
    expect(screen.getByRole("button", { name: "Copy remote thread ID" })).toBeTruthy();
    expect(api.openRemoteHandoffThread).not.toHaveBeenCalled();
  });
  it("contains link lookup and opener failures without blocking result actions", async () => {
    store.records = [record()];
    vi.mocked(api.openRemoteHandoffThread).mockRejectedValue(new Error("opener failed"));
    const instance = view();
    fireEvent.click(await screen.findByRole("button", { name: "Open in Codex App" }));
    expect(await screen.findByText(i18n.t("handoff.openThreadFailed"))).toBeTruthy();
    expect(screen.getByRole("button", { name: "Copy summary" })).toBeTruthy();
    instance.unmount();
    vi.mocked(api.getRemoteHandoffThreadLink).mockRejectedValue(new Error("metadata unavailable"));
    view();
    expect(await screen.findByText(i18n.t("handoff.threadLinkFailed"))).toBeTruthy();
    expect(screen.getByRole("button", { name: "Copy remote thread ID" })).toBeTruthy();
  });
  it("copies the current summary, diffstat and unapplied status as plain text", async () => {
    store.records = [record()];
    const copy = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal("navigator", { clipboard: { writeText: copy } });
    view();
    fireEvent.click(screen.getByRole("button", { name: "Copy summary" }));
    await waitFor(() => expect(copy).toHaveBeenCalledWith(handoffSummary(store.records[0]!, (key) => i18n.t(key))));
    const text = handoffSummary(store.records[0]!, (key) => i18n.t(key));
    expect(text).toContain("Ready for review");
    expect(text).toContain("wordcount.py\t+46\t−1");
    expect(text).toContain("Results have not been applied to local files");
  });
  it("keeps summary status accurate after apply or when no diff has been fetched", () => {
    const row = record();
    row.result.outcome = "applied";
    expect(handoffSummary(row, (key) => i18n.t(key))).toContain("Result status: Applied");
    row.result.outcome = null;
    row.result.fetched = false;
    const pending = handoffSummary(row, (key) => i18n.t(key));
    expect(pending).toContain("File changes have not been retrieved yet.");
    expect(pending).not.toContain("wordcount.py");
    row.result.fetched = true;
    row.result.capture!.partial = true;
    row.result.capture!.diffstat = [{ path: "asset.dat", added: null, deleted: null }];
    const partial = handoffSummary(row, (key) => i18n.t(key));
    expect(partial).toContain(i18n.t("handoff.partialCapture"));
    expect(partial).toContain("asset.dat\tBinary");
  });
  it("provides the same remote-open action in completion notifications", async () => {
    store.records = [record()];
    render(<HandoffNotifications onReview={vi.fn()} />);
    fireEvent.click(await screen.findByRole("button", { name: "Open in Codex App" }));
    await waitFor(() => expect(api.openRemoteHandoffThread).toHaveBeenCalledWith("handoff"));
    expect(screen.getByRole("button", { name: "Copy remote thread ID" })).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Copy summary" })).toBeNull();
  });
  it("prefills only the selected thread's draft and preserves edits across OS rechecks", async () => {
    const draft = { task: "Finish the CLI and tests.", originalRequest: "Create a word-count package.", lastProgress: "Parser is done." };
    vi.mocked(api.preflightRemoteHandoff).mockResolvedValue({ thread: { ...thread, draft }, changes: [], blockers: [], warnings: [], remoteOs: "linux", localRefNotice: "ref" });
    view();
    const brief = await screen.findByRole("textbox") as HTMLTextAreaElement;
    await waitFor(() => expect(brief.value).toBe(handoffDraft(draft, (key) => i18n.t(key))));
    expect(brief.value.startsWith(draft.task)).toBe(true);
    expect(brief.value).toContain(draft.originalRequest);
    expect(brief.value).toContain(draft.lastProgress);
    fireEvent.change(brief, { target: { value: "Edited task." } });
    fireEvent.change(screen.getByRole("combobox"), { target: { value: "linux" } });
    await waitFor(() => expect(api.preflightRemoteHandoff).toHaveBeenCalledWith("host", "local", { requiredOs: "linux" }));
    expect((await screen.findByRole("textbox") as HTMLTextAreaElement).value).toBe("Edited task.");
  });
  it("compose uses a thread list and requires an editable task before sending", async () => {
    const start = vi.spyOn(api, "startRemoteHandoff").mockResolvedValue(record("running"));
    view();
    fireEvent.click(await screen.findByRole("radio"));
    const brief = await screen.findByRole("textbox");
    expect((screen.getByRole("button", { name: "Hand off" }) as HTMLButtonElement).disabled).toBe(true);
    fireEvent.change(brief, { target: { value: "Implement top_words.\n\nBackground: parser is done." } });
    fireEvent.click(screen.getByRole("button", { name: "Hand off" }));
    await waitFor(() => expect(start).toHaveBeenCalledWith("host", "local", "Implement top_words.\n\nBackground: parser is done.", {}));
  });
  it("blocked shows its own localized reason and refuses submission", async () => {
    vi.mocked(api.preflightRemoteHandoff).mockResolvedValue({ thread, changes: [], blockers: ["HandoffPlatformMismatch: needs windows"], warnings: [], remoteOs: "linux", localRefNotice: "ref" });
    view(); fireEvent.click(await screen.findByRole("radio"));
    expect(await screen.findByText(i18n.t("handoff.errors.HandoffPlatformMismatch"))).toBeTruthy();
    expect((screen.getByRole("button", { name: "Hand off" }) as HTMLButtonElement).disabled).toBe(true);
  });
  it("running shows remote progress and offers a partial capture without applying it", async () => {
    store.records = [record("running")];
    const capture = vi.spyOn(api, "captureRemoteHandoff").mockResolvedValue(store.records[0]!);
    view();
    expect(screen.getByText("remote-thread")).toBeTruthy();
    expect(screen.getByText("Implement")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Apply to local worktree" })).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Bring back current progress" }));
    await waitFor(() => expect(capture).toHaveBeenCalledWith("handoff"));
  });
  it("back displays diffstat and applies only through the guarded backend", async () => {
    store.records = [record()];
    const settle = vi.spyOn(api, "settleRemoteHandoff").mockResolvedValue(record());
    view(); expect(screen.getByText("wordcount.py")).toBeTruthy(); expect(screen.getByText("+46")).toBeTruthy();
    fireEvent.click(await screen.findByRole("button", { name: "Apply to local worktree" }));
    await waitFor(() => expect(settle).toHaveBeenCalledWith("handoff", false));
  });
  it("drift only permits saving a branch, with no force-apply action", async () => {
    store.records = [record()];
    vi.mocked(api.reviewRemoteHandoff).mockResolvedValue({ canApply: false, patch: "+ change" });
    const settle = vi.spyOn(api, "settleRemoteHandoff").mockResolvedValue(record());
    view();
    expect(await screen.findByText(i18n.t("handoff.drift"))).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Apply to local worktree" })).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Save as branch" }));
    await waitFor(() => expect(settle).toHaveBeenCalledWith("handoff", true));
  });
  it("completion inbox survives reopening and is acknowledged explicitly", async () => {
    store.records = [record()];
    const acknowledge = vi.spyOn(api, "acknowledgeRemoteHandoff").mockResolvedValue(undefined);
    const review = vi.fn();
    const instance = render(<HandoffNotifications onReview={review} />);
    fireEvent.click(screen.getByRole("button", { name: "Review result" }));
    expect(review).toHaveBeenCalledWith("host", "handoff");
    expect(acknowledge).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Dismiss" }));
    await waitFor(() => expect(acknowledge).toHaveBeenCalledWith("handoff"));
    instance.unmount();
    store.records[0]!.result.acknowledged = true;
    render(<HandoffNotifications onReview={review} />);
    expect(screen.queryByRole("button", { name: "Review result" })).toBeNull();
  });
  it("keeps all four locale contracts and never labels a partial stop completed", () => {
    expect(() => assertResourceParity()).not.toThrow();
    expect(handoffState(record("interrupted"))).toBe("partial");
    expect(handoffState(record("failed"))).toBe("partial");
  });
});
