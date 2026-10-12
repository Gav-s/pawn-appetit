import { createFileRoute } from "@tanstack/react-router";
import { useAtomValue } from "jotai";
import DeepAnalysisPage from "@/features/deep-analysis/DeepAnalysisPage";
import { deepAnalysisService } from "@/features/deep-analysis/service";
import { enginesAtom } from "@/state/atoms";

function DeepAnalysisRoute() {
  const engines = useAtomValue(enginesAtom);
  return (
    <DeepAnalysisPage
      service={deepAnalysisService}
      installedEngines={engines
        .filter((engine) => engine.type === "local")
        .map(({ name, path }) => ({ name, path }))}
    />
  );
}

export const Route = createFileRoute("/deep-analysis")({
  component: DeepAnalysisRoute,
});
