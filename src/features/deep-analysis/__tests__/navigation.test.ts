import { describe, expect, it } from "vitest";
import { appNavigationItems } from "@/app/navigation";
import { Route } from "@/routes/deep-analysis";

describe("Deep Analysis navigation", () => {
    it("has its own workbench destination rather than opening the interactive analysis board", () => {
        expect(appNavigationItems.find((item) => item.id === "deepAnalysis")).toMatchObject({
            section: "workbench",
            url: "/deep-analysis",
        });
        expect(Route.options.component).toBeTypeOf("function");
    });
});
