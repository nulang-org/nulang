/**
 * Minimal semver 2.0.0 precedence comparison for registry version lists.
 *
 * Implements the precedence rules of semver.org §11:
 * - major/minor/patch compare numerically
 * - build metadata (`+meta`) is ignored
 * - a release version has higher precedence than any prerelease of it
 * - prerelease identifiers compare dot-separated: numeric identifiers
 *   numerically, alphanumeric identifiers lexicographically (ASCII), and
 *   numeric identifiers always have lower precedence than alphanumeric ones
 * - a larger set of prerelease fields has higher precedence than a smaller
 *   set when all preceding identifiers are equal
 *
 * Parsing is deliberately lenient: registry keys are client-supplied and may
 * not be valid semver (e.g. a "latest" tag), and the list endpoint must not
 * fail because of them. Invalid versions compare by their numeric prefix
 * where possible and otherwise fall back to 0 components.
 */

function parseSemver(v: string) {
  const [versionCore, ...prereleaseParts] = v.split('+')[0].split('-');
  const prerelease = prereleaseParts.join('-');
  const parts = versionCore.split('.').map((num) => parseInt(num, 10) || 0);
  while (parts.length < 3) {
    parts.push(0);
  }
  return {
    major: parts[0],
    minor: parts[1],
    patch: parts[2],
    prerelease,
  };
}

/**
 * Compare two prerelease strings per semver §11.4. Both are non-empty.
 */
function comparePrerelease(a: string, b: string): number {
  const ai = a.split('.');
  const bi = b.split('.');
  const len = Math.max(ai.length, bi.length);
  for (let i = 0; i < len; i++) {
    if (i >= ai.length) return -1; // fewer fields => lower precedence
    if (i >= bi.length) return 1;
    const x = ai[i];
    const y = bi[i];
    const xn = /^\d+$/.test(x);
    const yn = /^\d+$/.test(y);
    if (xn && yn) {
      const nx = parseInt(x, 10);
      const ny = parseInt(y, 10);
      if (nx !== ny) return nx - ny;
    } else if (xn !== yn) {
      return xn ? -1 : 1; // numeric identifiers have lower precedence
    } else if (x !== y) {
      return x < y ? -1 : 1; // ASCII lexicographic
    }
  }
  return 0;
}

/**
 * Compare two semver strings (e.g. "0.10.0" > "0.9.0", "1.0.0" > "1.0.0-alpha",
 * "1.0.0-alpha.10" > "1.0.0-alpha.9").
 */
export function compareSemver(a: string, b: string): number {
  const pa = parseSemver(a);
  const pb = parseSemver(b);

  if (pa.major !== pb.major) return pa.major - pb.major;
  if (pa.minor !== pb.minor) return pa.minor - pb.minor;
  if (pa.patch !== pb.patch) return pa.patch - pb.patch;

  // Versions without pre-release have higher precedence than versions with pre-release
  if (!pa.prerelease && pb.prerelease) return 1;
  if (pa.prerelease && !pb.prerelease) return -1;
  if (pa.prerelease && pb.prerelease) {
    return comparePrerelease(pa.prerelease, pb.prerelease);
  }

  return 0;
}

/** Return a new array sorted by semver precedence (input is not mutated). */
export function sortSemver(versions: string[]): string[] {
  return [...versions].sort(compareSemver);
}
