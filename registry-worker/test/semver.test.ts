import { describe, expect, it } from 'vitest';
import { compareSemver, sortSemver } from '../src/semver';

describe('compareSemver', () => {
  it('orders numeric components numerically, not lexicographically', () => {
    expect(compareSemver('0.10.0', '0.9.0')).toBeGreaterThan(0);
    expect(compareSemver('1.2.10', '1.2.9')).toBeGreaterThan(0);
    expect(compareSemver('1.0.0', '0.99.99')).toBeGreaterThan(0);
    expect(compareSemver('0.9.0', '0.10.0')).toBeLessThan(0);
  });

  it('ignores build metadata', () => {
    expect(compareSemver('1.0.0+meta', '1.0.0')).toBe(0);
    expect(compareSemver('1.0.0+build.5', '1.0.0+build.9')).toBe(0);
  });

  it('gives release versions precedence over prereleases', () => {
    expect(compareSemver('1.0.0', '1.0.0-alpha')).toBeGreaterThan(0);
    expect(compareSemver('1.0.0-alpha', '1.0.0')).toBeLessThan(0);
  });

  it('compares prerelease numeric identifiers numerically', () => {
    expect(compareSemver('1.0.0-alpha.10', '1.0.0-alpha.9')).toBeGreaterThan(0);
    expect(compareSemver('1.0.0-rc.1', '1.0.0-rc.10')).toBeLessThan(0);
    expect(compareSemver('1.0.0-2', '1.0.0-10')).toBeLessThan(0);
  });

  it('compares prerelease alphanumeric identifiers lexicographically', () => {
    expect(compareSemver('1.0.0-alpha', '1.0.0-beta')).toBeLessThan(0);
    expect(compareSemver('1.0.0-beta', '1.0.0-alpha')).toBeGreaterThan(0);
  });

  it('gives numeric prerelease identifiers lower precedence than alphanumeric', () => {
    expect(compareSemver('1.0.0-alpha.1', '1.0.0-alpha.beta')).toBeLessThan(0);
  });

  it('gives a larger prerelease field set higher precedence', () => {
    expect(compareSemver('1.0.0-alpha', '1.0.0-alpha.1')).toBeLessThan(0);
    expect(compareSemver('1.0.0-alpha.1', '1.0.0-alpha')).toBeGreaterThan(0);
  });

  it('handles multi-dot prereleases', () => {
    expect(compareSemver('1.0.0-alpha.1.2', '1.0.0-alpha.1.10')).toBeLessThan(0);
    expect(compareSemver('1.0.0-alpha.1.2', '1.0.0-alpha.1.2')).toBe(0);
  });

  it('is lenient with non-semver input instead of throwing', () => {
    expect(() => compareSemver('v1.2.3', '1.0.0')).not.toThrow();
    expect(() => compareSemver('latest', '1.0.0')).not.toThrow();
  });
});

describe('sortSemver', () => {
  it('sorts a mixed list by semver precedence', () => {
    const input = [
      '1.0.0-alpha.10',
      '0.9.0',
      '1.0.0',
      '0.10.0',
      '1.0.0-alpha.9',
      '1.0.0-beta',
    ];
    expect(sortSemver(input)).toEqual([
      '0.9.0',
      '0.10.0',
      '1.0.0-alpha.9',
      '1.0.0-alpha.10',
      '1.0.0-beta',
      '1.0.0',
    ]);
  });

  it('does not mutate the input array', () => {
    const input = ['0.10.0', '0.9.0'];
    sortSemver(input);
    expect(input).toEqual(['0.10.0', '0.9.0']);
  });
});
