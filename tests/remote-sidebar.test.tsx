import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import i18n from "i18next";
import "@/i18n";
import { Rail } from "@/components/Rail";
import { RemoteSidebarPreference } from "@/components/RemoteSidebarPreference";
import { accessibleScreen, visibleScreens } from "@/screens/registry";
afterEach(cleanup);
describe("Remote sidebar", () => {
  it("hides an unused tab and updates its shortcut slots", async () => {
    await i18n.changeLanguage("en");
    const props = { active:"today" as const, onNavigate:vi.fn(), onOpenUpdates:vi.fn(), attention:{}, updateAttention:false };
    const view = render(<Rail {...props} remoteVisible={false} />);
    expect(screen.queryByRole("button",{name:i18n.t("navigation.remote.label")})).toBeNull();
    expect(visibleScreens(false).some((item) => item.id === "remote")).toBe(false);
    expect(accessibleScreen("remote",false)).toBe("today");
    view.rerender(<Rail {...props} remoteVisible />);
    fireEvent.click(screen.getByRole("button",{name:i18n.t("navigation.remote.label")}));
    expect(props.onNavigate).toHaveBeenCalledWith("remote");
    expect(accessibleScreen("remote",true)).toBe("remote");
  });
  it("offers automatic, always-show and hide, and explains the current choice", async () => {
    await i18n.changeLanguage("en");
    const change = vi.fn();
    const view = render(<RemoteSidebarPreference mode="auto" onChange={change} />);
    expect(screen.getByRole("button",{name:i18n.t("settings.remoteSidebar.mode.auto")}).getAttribute("aria-pressed")).toBe("true");
    expect(screen.getByText(i18n.t("settings.remoteSidebar.hint.auto"))).toBeTruthy();
    fireEvent.click(screen.getByRole("button",{name:i18n.t("settings.remoteSidebar.mode.show")}));
    expect(change).toHaveBeenCalledWith("show");
    fireEvent.click(screen.getByRole("button",{name:i18n.t("settings.remoteSidebar.mode.hide")}));
    expect(change).toHaveBeenCalledWith("hide");
    view.rerender(<RemoteSidebarPreference mode="hide" onChange={change} />);
    expect(screen.getByText(i18n.t("settings.remoteSidebar.hint.hide"))).toBeTruthy();
  });
});
