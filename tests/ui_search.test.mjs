import assert from "node:assert/strict";
import test from "node:test";
import { findMatches } from "../src/ui/search.mjs";

test("finds literal case-insensitive matches, including regex characters", () => {
  assert.deepEqual(findMatches("a.b A.B a-b", "a.b"), [
    { start: 0, end: 3 },
    { start: 4, end: 7 },
  ]);
  assert.deepEqual(findMatches("hello", ""), []);
});

test("match offsets refer to the original Unicode text", () => {
  assert.deepEqual(findMatches("İ xx foo FOO", "foo"), [
    { start: 5, end: 8 },
    { start: 9, end: 12 },
  ]);
  assert.deepEqual(findMatches("😀 fish 😀", "😀"), [
    { start: 0, end: 2 },
    { start: 8, end: 10 },
  ]);
});
