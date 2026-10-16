import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import i18n from "i18next";
import "@/i18n";
import { api } from "@/lib/api";
import { RemoteHandoff } from "@/components/RemoteHandoff";
import { HandoffNotifications } from "@/components/HandoffNotifications";
import { handoffState, type RemoteHandoffRecord, type RemoteHandoffThread } from "@/lib/remoteHandoff";
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
});
afterEach(() => { cleanup(); vi.restoreAllMocks(); });
const view = () => render(<RemoteHandoff hostId="host" hostName="Jetson" active />);
describe("remote handoff", () => {
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
