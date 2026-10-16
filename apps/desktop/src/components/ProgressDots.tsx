/** Step indicator shared by the first-run onboarding and the sync setup wizard. */
export function ProgressDots({ current, total }: { current: number; total: number }) {
  return (
    <div className="flex items-center justify-center gap-2 mb-6">
      {Array.from({ length: total }, (_, i) => (
        <div
          key={i}
          className="rounded-full transition-all duration-300"
          style={{
            width: i === current ? 10 : i < current ? 8 : 6,
            height: i === current ? 10 : i < current ? 8 : 6,
            backgroundColor:
              i === current
                ? "var(--primary)"
                : i < current
                  ? "color-mix(in oklch, var(--primary) 50%, transparent)"
                  : "var(--muted)",
            transform: i === current ? "scale(1.1)" : "scale(1)",
          }}
        />
      ))}
    </div>
  );
}
