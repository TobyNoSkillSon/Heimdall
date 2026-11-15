#!/usr/bin/env python3
"""Opt-in checks of the native Qwen3-Reranker mode (`heimdall-embedding-worker --rerank <model dir>`).

  --tokenizer   Swift token IDs (`--tokenize`) against Python `tokenizers`/`transformers` (no inference).
  (default)     Parity: Swift helper vs the lab's Python MLX reference on 3 queries x 20 documents
                (English and Polish, relevant, unrelated, empty). Pass: max |dp| <= 0.02, same top-1 per query.
  --measure     One 100-document request (~60-token docs), a batch-size sweep (8/16/32/64 rows, interleaved,
                100 docs of ~60-200 tokens) and one request each at 50/100/200 docs. Informational: Mac in use.

Model inference; run one at a time. Needs mlx-lm (e.g. `uv run --offline --no-project --with mlx-lm==0.32.2 python
rerank_parity.py`). The Swift helper and the Python reference run one after the other, never together.
"""
import argparse
import json
import random
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
LAB = Path('/Users/user/Work/Projects/Heimdall/lab/reranker-2026-11-13')
MODEL = LAB / 'models/qwen3-reranker-0.6b'
BINARY = HERE.parents[3] / 'models/embeddinggemma2/native/heimdall-embedding-worker'  # beside mlx.metallib

QUERIES = [
    ('When did we decide to cap the MLX cache for the embedder?', [
        'We capped the MLX buffer cache at 256 MB on 30 October after the embedder held 3.7 GB.',
        'User said the embedder should give memory back after every request, so the cache is cleared now.',
        'The pizza place on Garbary closes at ten on weekdays.',
        '',
        'Ustaliliśmy, że limit pamięci podręcznej MLX dla embeddera to 256 MB.',
        'Swap grew by 830 MB during the uncapped reranker run and paused the embedder.',
        'Let me check the weather in Poznań for tomorrow morning.',
        'cache',
        'The new build of Vella transcribes Polish faster than before.',
        'We talked about the release notes for Arsenal and the updater.',
        'Memory.cacheLimit is set before weights load; the default was min(2 GiB, free/16).',
        'Jutro idę do dentysty o dziewiątej.',
        'The embedder pauses when swap grows by more than 16 MB within two minutes.',
        '   ',
        'I bought a new keyboard, the one with brown switches.',
        'On 30 Oct we measured the 256 MB cap: long texts about 17% slower, vectors bit-identical.',
        'Heimdall said the battery is at 40 percent.',
        'Zmniejszyliśmy cache, bo helper trzymał martwe bufory na GPU.',
        'The meeting with the recruiter is on Thursday at 14:00.',
        'GPU buffers of finished requests stayed resident until the next request.',
    ]),
    ('Kiedy jest spotkanie z rekruterem?', [
        'Spotkanie z rekruterką z Acme jest w czwartek o czternastej.',
        'The recruiter call moved to Thursday afternoon, 2 pm.',
        'Rozmowa rekrutacyjna odbędzie się zdalnie przez Teams.',
        '',
        'Kupiłem nowe słuchawki, ale lewa nie działa.',
        'Pogoda w Warszawie: deszcz, dwanaście stopni.',
        'User asked Heimdall to remind him about the interview an hour before.',
        'Zażółć gęślą jaźń — test polskich znaków.',
        'The reranker test showed 39 of 60 right-moment-first answers.',
        'W piątek mam wizytę u lekarza o 8:30.',
        'Recruiter: Anna Kowalska, Acme, backend role in Poznań.',
        'spotkanie',
        'We fixed the bug where the phone mic stayed open after the call.',
        'Ugotowałem pierogi ruskie, wyszły świetnie.',
        'Przesunęli spotkanie z rekruterem z środy na czwartek.',
        'The CV in Polish needs the new project section.',
        'Nie pamiętam, czy wysłałem CV do tej firmy.',
        'Build failed because the Metal toolchain was missing.',
        'Kalendarz: czwartek 14:00 — rozmowa, Acme.',
        'Heimdall read the email from the recruiter aloud.',
    ]),
    ('what model does the archive search use for reranking', [
        'Qwen3-Reranker-0.6B on the top 50 to 100 hits won the reranker test.',
        'Jina v3.5 scored 32 of 60 at 50 hits and got worse at 200 and 500.',
        'Embedding alone put the right moment first in 24 of 60 questions.',
        '',
        'Reranker to model, który ponownie ocenia wyniki wyszukiwania.',
        'The archive search embeds the query with EmbeddingGemma 2 and scans the vectors.',
        'I want lasagne for dinner tonight.',
        'User wanted the reranker to run locally on battery, cheaply.',
        'The pixel-art sprites need a darker outline.',
        'Reranking 500 hits is not better (36 of 60) and costs 1.3 seconds.',
        'Model rerankujący działa w BF16 na MLX, bez kwantyzacji.',
        'rerank',
        'The TV remote pairing failed twice yesterday.',
        'Weights stay BF16 exactly as stored, no FP16 cast.',
        'Today the Rust scan takes about 27 seconds per meaning search.',
        'Kot zrzucił kubek ze stołu.',
        'A cross-encoder reads the query and the document together and outputs relevance.',
        'Heimdall memory search returns transcript chunks with timestamps.',
        'We should buy more coffee.',
        'The reranker scores yes versus no on the last token.',
    ]),
]


def swap_mb():
    out = subprocess.run(['sysctl', '-n', 'vm.swapusage'], capture_output=True, text=True).stdout
    return float(out.split('used = ')[1].split('M')[0])


class Helper:
    def __init__(self, binary):
        self.p = subprocess.Popen([str(binary), '--rerank', str(MODEL)], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=sys.stderr, text=True)
        self.n = 0

    def ask(self, **request):
        self.n += 1
        self.p.stdin.write(json.dumps({'id': self.n, **request}) + '\n'); self.p.stdin.flush()
        reply = json.loads(self.p.stdout.readline())
        assert reply.get('id') == self.n, reply
        if 'error' in reply:
            raise RuntimeError(reply['error'])
        return reply

    def close(self):
        self.p.stdin.close(); self.p.wait(30)


def tokenizer_check(binary):
    from transformers import AutoTokenizer
    sys.path.insert(0, str(LAB))
    from qwen_reranker import PREFIX, SUFFIX, INSTRUCT
    tok = AutoTokenizer.from_pretrained(str(MODEL))
    texts = ['Zażółć gęślą jaźń', ' Ładne „cudzysłowy” i źdźbło — ŻÓŁW, ąęćńóśźż', ' Spotkanie z rekruterem w czwartek o 14:00.',
             'Hello, world! Tabs\tand  double  spaces\nnew line', ' é combining and emoji 👩‍💻 ok', '', SUFFIX,
             PREFIX + f'<Instruct>: {INSTRUCT}\n<Query>: Kiedy jest spotkanie?\n<Document>:']
    texts += [' ' + d for _, docs in QUERIES for d in docs]
    out = subprocess.run([str(binary), '--tokenize', str(MODEL)], input=''.join(json.dumps(t) + '\n' for t in texts),
                         capture_output=True, text=True, check=True).stdout.splitlines()
    bad = 0
    for text, line in zip(texts, out, strict=True):
        swift = json.loads(line)
        if swift != tok.encode(text, add_special_tokens=False) or swift != tok.encode(text, add_special_tokens=True):
            bad += 1; print('MISMATCH', repr(text[:60]), swift[:12], tok.encode(text, add_special_tokens=False)[:12])
    print(json.dumps({'tokenizer_texts': len(texts), 'mismatches': bad}))
    return bad == 0


def parity(binary):
    swap0 = swap_mb()
    h = Helper(binary)
    # Same batch size as the reference: batch composition changes padding and so BF16 rounding.
    swift = [h.ask(query=q, documents=d, batch=32) for q, d in QUERIES]
    h.close()
    swap1 = swap_mb()
    if swap1 > swap0 + 16:
        sys.exit(f'swap grew {swap0:.0f} -> {swap1:.0f} MB; stopped before the Python reference')
    sys.path.insert(0, str(LAB))
    from qwen_reranker import QwenReranker
    ref = QwenReranker(batch=32, max_doc_tokens=256)
    python = [ref.score(q, d) for q, d in QUERIES]
    rows, worst, top_same = [], 0.0, True
    for (q, docs), s, p in zip(QUERIES, swift, python):
        a = s['scores']
        diff = max(abs(x - y) for x, y in zip(a, p))
        worst_doc = max(range(len(a)), key=lambda i: abs(a[i] - p[i]))
        worst = max(worst, diff)
        top_s, top_p = max(range(len(a)), key=a.__getitem__), max(range(len(p)), key=p.__getitem__)
        same_rank = sorted(range(len(a)), key=lambda i: -a[i]) == sorted(range(len(p)), key=lambda i: -p[i])
        top_same &= top_s == top_p
        rows.append({'query': q, 'max_abs_diff': round(diff, 5), 'top1_swift': top_s, 'top1_python': top_p,
                     'full_ranking_same': same_rank, 'worst_doc': [worst_doc, round(a[worst_doc], 4), round(p[worst_doc], 4)], 'swift_top1_p': round(a[top_s], 4), 'python_top1_p': round(p[top_p], 4)})
    print(json.dumps({'rows': rows, 'max_abs_diff': round(worst, 5), 'top1_all_same': top_same,
                      'swap_mb': [swap0, swap_mb()], 'model_version': swift[0]['model_version']}, indent=1))
    return worst <= 0.02 and top_same


def synthetic_docs(n, low, high, seed):
    from transformers import AutoTokenizer
    tok = AutoTokenizer.from_pretrained(str(MODEL))
    pool = [d for _, docs in QUERIES for d in docs if d.strip()]
    rng = random.Random(seed)
    out = []
    for _ in range(n):
        target, text = rng.randint(low, high), ''
        while len(tok.encode(' ' + text, add_special_tokens=False)) < target:
            text += (' ' if text else '') + rng.choice(pool)
        out.append(text)
    return out


def measure(binary):
    swap0 = swap_mb()
    docs60 = synthetic_docs(100, 55, 65, 1)
    mixed = synthetic_docs(100, 60, 200, 2)
    big = synthetic_docs(200, 60, 200, 3)
    q = QUERIES[0][0]
    h = Helper(binary)
    h.ask(query=q, documents=docs60[:8])  # warm-up
    one = h.ask(query=q, documents=docs60)['stats']
    result = {'label': 'Mac in use, informational', 'one_100x60': one}
    sizes = (8, 16, 32, 64)
    for b in sizes:  # warm-up per batch size
        h.ask(query=q, documents=mixed, batch=b)
    sweep = {b: [] for b in sizes}
    for _ in range(3):
        for b in sizes:
            s = h.ask(query=q, documents=mixed, batch=b)['stats']
            sweep[b].append((s['seconds'], s['peak_bytes']))
            if swap_mb() > swap0 + 16:
                h.close(); sys.exit('swap grew during the sweep; stopped')
    result['sweep_100_mixed'] = {b: {'median_s': round(sorted(x[0] for x in v)[1], 3), 'max_peak_mb': round(max(x[1] for x in v) / 2**20)}
                                 for b, v in sweep.items()}
    result['sizes'] = {}
    for n in (50, 100, 200):
        s = h.ask(query=q, documents=big[:n])['stats']
        result['sizes'][n] = {'seconds': round(s['seconds'], 3), 'peak_mb': round(s['peak_bytes'] / 2**20), 'tokens': s['tokens'], 'batch': s['batch']}
    result['usage'] = h.ask(op='usage')
    h.close()
    result['swap_mb'] = [swap0, swap_mb()]
    print(json.dumps(result, indent=1))


if __name__ == '__main__':
    ap = argparse.ArgumentParser()
    ap.add_argument('--binary', default=str(BINARY))
    ap.add_argument('--tokenizer', action='store_true')
    ap.add_argument('--measure', action='store_true')
    a = ap.parse_args()
    if a.tokenizer:
        sys.exit(0 if tokenizer_check(a.binary) else 1)
    if a.measure:
        measure(a.binary); sys.exit(0)
    sys.exit(0 if parity(a.binary) else 1)
