import mark from "../assets/vmax-mark.png";

export function VmaXLogo({
  size = 28,
  className = "",
}: {
  size?: number;
  className?: string;
}) {
  return (
    <img
      src={mark}
      width={size}
      height={size}
      alt="VmaX logo"
      className={`vmax-mark ${className}`.trim()}
    />
  );
}
