export const TOKEN_TTL_MINUTES = 30;

export function isExpired(issuedAt: Date): boolean {
  return Date.now() - issuedAt.getTime() > TOKEN_TTL_MINUTES * 60_000;
}
