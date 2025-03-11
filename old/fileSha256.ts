export async function fileSha256(filePath: string) {
  const hasher = new Bun.CryptoHasher("sha256");
  const stream = Bun.file(filePath).stream();
  for await (const chunk of stream) hasher.update(chunk);
  return hasher.digest("hex");
}
