export function BrandMark({ compact = false }: { compact?: boolean }) {
  return (
    <span
      className={compact ? "brand-mark brand-mark--compact" : "brand-mark"}
      aria-hidden="true"
    >
      <span />
      <span />
      <span />
      <span />
      <span />
    </span>
  );
}
