export type StepId = "project" | "session" | "agent" | "container" | "advanced" | "review";

export interface StepDef {
  id: StepId;
  label: string;
}

interface Props {
  steps: StepDef[];
  currentIndex: number;
}

export function StepIndicator({ steps, currentIndex }: Props) {
  return (
    <div className="flex items-center gap-1.5 mb-6">
      {steps.map((step, i) => (
        <div
          key={step.id}
          className={`h-2 rounded-full transition-all duration-300 ${
            i === currentIndex ? "w-6 bg-brand-600" : i < currentIndex ? "w-2 bg-green-500" : "w-2 bg-surface-700"
          }`}
          title={step.label}
        />
      ))}
    </div>
  );
}
