export function formatFileSize(size: number, fractionDigits = 1) {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let unitIndex = 0;
  while (size >= 1024 && unitIndex < units.length - 1) {
    size /= 1024;
    unitIndex++;
  }
  return `${size.toFixed(fractionDigits)} ${units[unitIndex]}`;
}
