"""Regenerates data/en.txt: about 10,000 English words, most frequent first.

Source: the merge order of the Whisper tokenizer (MIT licence; byte-pair merges are learned from the most frequent
character sequences of its training text, so the order of whole-word tokens approximates word frequency in
transcribed speech and web text). Only lowercase ASCII whole-word tokens that are dictionary words (or a regular
inflection of one) in /usr/share/dict/words are kept, which drops sub-word fragments such as "polit" or "estab".

Usage: python3 -I gen_en.py <whisper tokenizer.json> > en.txt
(the faster-whisper-base snapshot in the Hugging Face cache was used for the committed file)
"""
import json
import sys

LIMIT = 10_000
# Two-letter tokens are mostly fragments ("th", "re", "st"); keep only real words.
TWO = set("am an as at be by do go he hi if in is it me my no of oh ok on or so to up us we".split())
SUFFIXES = [("s", ""), ("es", ""), ("ed", ""), ("ed", "e"), ("ing", ""), ("ing", "e"), ("ly", ""), ("er", ""),
            ("ers", ""), ("ies", "y"), ("ied", "y"), ("est", ""), ("d", "")]

vocab = json.load(open(sys.argv[1]))["model"]["vocab"]
dictionary = {w.strip().lower() for w in open("/usr/share/dict/words")}


def is_word(w):
    if len(w) == 1:
        return w in ("a", "i")
    if len(w) == 2:
        return w in TWO
    if w in dictionary:
        return True
    return any(w.endswith(s) and len(w) - len(s) >= 2 and w[: -len(s)] + r in dictionary for s, r in SUFFIXES)


out, seen = [], set()
for token, _ in sorted(vocab.items(), key=lambda kv: kv[1]):
    if not token.startswith("Ġ"):
        continue
    w = token[1:]
    if w.isascii() and w.isalpha() and w.islower() and w not in seen and is_word(w):
        seen.add(w)
        out.append(w)
    if len(out) == LIMIT:
        break
sys.stdout.write("\n".join(out) + "\n")
