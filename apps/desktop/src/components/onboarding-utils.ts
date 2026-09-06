import type { OnboardingState } from "@/stores/appStore";

export interface OnboardingFlow {
  id: string;
  steps: string[];
  blocking: boolean;
  shouldShow: (state: OnboardingState) => boolean;
}

export const ONBOARDING_FLOWS: OnboardingFlow[] = [
  {
    id: "initial",
    steps: ["welcome", "audio", "ai", "sync", "ready"],
    blocking: true,
    shouldShow: (s) => !s.completedFlows["initial"],
  },
];

export function getActiveFlow(state: OnboardingState | undefined): OnboardingFlow | null {
  if (!state?.completedFlows) return null;
  return ONBOARDING_FLOWS.find((f) => f.shouldShow(state)) ?? null;
}

export interface StepNav {
  onNext: () => void;
  onBack: () => void;
  onFinish: () => void;
}
