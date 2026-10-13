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
  it("makes first-time setup visible through an explicit preference", async () => {
    await i18n.changeLanguage("en");
    const change = vi.fn();
    render(<RemoteSidebarPreference forced={false} onChange={change} />);
    fireEvent.click(screen.getByRole("switch",{name:i18n.t("settings.remoteSidebar.label")}));
    expect(change).toHaveBeenCalledWith(true);
    expect(screen.getByText(i18n.t("settings.remoteSidebar.hint"))).toBeTruthy();
  });
});
