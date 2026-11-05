"""Agentic benchmark tasks (easy -> hard). Hidden tests live in check(), never in the work dir.

Task(name, level, prompt, setup(dir), check(dir, stdout) -> bool)
"""
import hashlib
import json
import random
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Callable


@dataclass
class Task:
    name: str
    level: str  # easy | medium | hard
    prompt: str
    setup: Callable[[Path], None]
    check: Callable[[Path, str], bool]


def py(d: Path, *args: str, timeout=60) -> str:
    r = subprocess.run([sys.executable, *args], cwd=d, capture_output=True, text=True, timeout=timeout)
    return r.stdout + r.stderr


def snippet(d: Path, code: str, timeout=60) -> str:
    return py(d, "-c", code, timeout=timeout)


def digest(d: Path, files) -> str:
    return hashlib.sha256(b"".join((d / f).read_bytes() for f in sorted(files))).hexdigest()


def w(d: Path, rel: str, text: str):
    p = d / rel
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(text)


# ---------------------------------------------------------------- easy
def c_create(d, out):
    return py(d, "fizzbuzz.py").split() == ["1", "2", "Fizz", "4", "Buzz", "Fizz", "7", "8", "Fizz", "Buzz",
                                            "11", "Fizz", "13", "14", "FizzBuzz"]


def s_fix(d):
    w(d, "calc.py", "def average(xs):\n    return sum(xs) / (len(xs) + 1)\n")
    w(d, "test_calc.py", "from calc import average\nassert average([2, 4, 6]) == 4\nassert average([5]) == 5\nprint('ok')\n")


def c_fix(d, out):
    return py(d, "test_calc.py").strip() == "ok"


def s_search(d):
    for i in range(12):
        w(d, f"mod{i}.py", f"def helper_{i}():\n    return {i}\n")
    w(d, "mod7.py", "def helper_7():\n    return 7\n\n\ndef secret_token_generator():\n    return 'x'\n")


def s_refactor(d):
    w(d, "lib.py", "def old_name(x):\n    return x * 2\n")
    w(d, "a.py", "from lib import old_name\nprint(old_name(1))\n")
    w(d, "b.py", "import lib\nprint(lib.old_name(2))\n")
    w(d, "c.py", "from lib import old_name\nprint(old_name(3))\n")


def c_refactor(d, out):
    src = "".join((d / f).read_text() for f in ["lib.py", "a.py", "b.py", "c.py"])
    return "old_name" not in src and "new_name" in src and [py(d, f).strip() for f in ("a.py", "b.py", "c.py")] == ["2", "4", "6"]


def c_stats(d, out):
    return snippet(d, "import stats; assert stats.median([3,1,2])==2; assert stats.median([4,1,3,2])==2.5; "
                      "assert stats.median([7])==7; print('ok')").strip() == "ok"


# ---------------------------------------------------------------- medium
def s_multibug(d):
    w(d, "pkg/__init__.py", "")
    w(d, "pkg/textutil.py", "def title_words(s):\n    return ' '.join(w.capitalize() for w in s.split(' '))\n\n"
                            "def count_vowels(s):\n    return sum(1 for ch in s if ch in 'aeiou')\n")
    w(d, "pkg/mathutil.py", "def clamp(x, lo, hi):\n    if x < lo:\n        return lo\n    if x > hi:\n        return lo\n    return x\n\n"
                            "def mean(xs):\n    return sum(xs) / len(xs) if xs else 0.0\n")
    w(d, "pkg/lists.py", "def chunk(xs, n):\n    return [xs[i:i + n] for i in range(0, len(xs) - n, n)]\n\n"
                         "def dedupe(xs):\n    seen = set()\n    out = []\n    for x in xs:\n        if x not in seen:\n            seen.add(x)\n            out.append(x)\n    return out\n")
    w(d, "run_tests.py",
      "from pkg.textutil import title_words, count_vowels\nfrom pkg.mathutil import clamp, mean\nfrom pkg.lists import chunk, dedupe\n"
      "fails = []\n"
      "def check(name, got, want):\n    if got != want:\n        fails.append(f'{name}: got {got!r} want {want!r}')\n"
      "check('title', title_words('hello big world'), 'Hello Big World')\n"
      "check('vowels', count_vowels('AEIOU xyz'), 5)\n"
      "check('clamp_hi', clamp(15, 0, 10), 10)\ncheck('clamp_lo', clamp(-5, 0, 10), 0)\ncheck('clamp_mid', clamp(5, 0, 10), 5)\n"
      "check('mean', mean([1, 2, 3]), 2.0)\n"
      "check('chunk', chunk([1, 2, 3, 4, 5], 2), [[1, 2], [3, 4], [5]])\n"
      "check('dedupe', dedupe([3, 1, 3, 2, 1]), [3, 1, 2])\n"
      "print('ALL PASS' if not fails else '\\n'.join(fails))\n")


def c_multibug(d, out):
    return py(d, "run_tests.py").strip() == "ALL PASS" and "check('vowels'" in (d / "run_tests.py").read_text() \
        and "check('chunk'" in (d / "run_tests.py").read_text()


def s_lru(d):
    w(d, "SPEC.md", "# LRUCache\nImplement class `LRUCache` in `cache.py`.\n\n- `LRUCache(capacity)`; capacity >= 1 else ValueError.\n"
                    "- `get(key, default=None)`: returns value and marks key most-recently-used.\n"
                    "- `put(key, value)`: inserts/updates, marks most-recently-used; evicts least-recently-used when over capacity.\n"
                    "- `len(cache)` returns size; `key in cache` does NOT change recency.\n"
                    "- `keys()` returns keys ordered least-recent -> most-recent.\n"
                    "- `delete(key)` removes key, returns True if existed else False.\n")


def c_lru(d, out):
    code = """
from cache import LRUCache
try:
    LRUCache(0); raise SystemExit('no ValueError')
except ValueError: pass
c = LRUCache(2); c.put('a',1); c.put('b',2)
assert c.get('a')==1
c.put('c',3)
assert 'b' not in c and 'a' in c and 'c' in c and len(c)==2
assert c.get('zzz','d')=='d'
assert c.keys()==['a','c']
assert ('a' in c) and c.keys()==['a','c']
c.put('a',9); assert c.keys()==['c','a'] and c.get('a')==9
assert c.delete('c') is True and c.delete('c') is False and len(c)==1
print('ok')
"""
    return snippet(d, code).strip() == "ok"


def s_bigfile(d):
    lines = []
    for i in range(3000):
        k = 7919 if i == 2371 else i * 3 + 1
        lines.append(f"def f_{i}(x):\n    # block {i}\n    return x + {i * 3 + 1}\n" if i != 2371 else
                     f"def f_{i}(x):\n    # block {i}\n    return x + {k}\n")
    w(d, "big.py", "\n".join(lines) + "\nALL = [" + ", ".join(f"f_{i}" for i in range(3000)) + "]\n")
    w(d, "test_big.py", "from big import ALL\nbad = [i for i, f in enumerate(ALL) if f(10) != 10 + i * 3 + 1]\n"
                        "print('ok' if not bad else f'FAIL: {len(bad)} functions wrong')\n")


def c_bigfile(d, out):
    return py(d, "test_big.py").strip() == "ok" and (d / "big.py").read_text().count("def f_") == 3000


def s_repo_search(d):
    rnd = random.Random(1)
    for i in range(400):
        w(d, f"src/m{i % 20}/sub{i % 7}/file_{i}.py", f"VALUE_{i} = {rnd.randint(0, 999)}\n\ndef fn_{i}():\n    return VALUE_{i}\n")
    w(d, "src/m13/sub4/file_333.py", "VALUE_333 = 5\nRETRY_BUDGET = 17\n\ndef fn_333():\n    return RETRY_BUDGET\n")


def c_repo_search(d, out):
    return "17" in out and "file_333" in out


def s_csv(d):
    rnd = random.Random(7)
    cats = ["food", "rent", "fun", "travel", "tools"]
    rows = ["date,category,amount"] + [f"2024-01-{(i % 28) + 1:02d},{rnd.choice(cats)},{rnd.randint(1, 500)}.{rnd.randint(0, 99):02d}" for i in range(500)]
    w(d, "data.csv", "\n".join(rows) + "\n")


def c_csv(d, out):
    import csv
    tot = {}
    for r in csv.DictReader(open(d / "data.csv")):
        tot[r["category"]] = tot.get(r["category"], 0) + float(r["amount"])
    want = [(k, v) for k, v in sorted(tot.items(), key=lambda kv: -kv[1])]
    try:
        got = [l.strip().split(",") for l in (d / "report.txt").read_text().strip().splitlines()]
        return [g[0] for g in got] == [k for k, _ in want] and all(abs(float(g[1]) - v) < 0.01 for g, (_, v) in zip(got, want))
    except Exception:
        return False


def s_git(d):
    subprocess.run("git init -q -b main && git config user.email a@b.c && git config user.name t && "
                   "echo 'print(\"hi\")' > app.py && git add . && git commit -qm init", shell=True, cwd=d, check=True)


def c_git(d, out):
    def g(c):
        return subprocess.run(c, shell=True, cwd=d, capture_output=True, text=True).stdout.strip()
    return g("git rev-parse --abbrev-ref HEAD") == "feature/greet" and "greeting" in g("git log -1 --pretty=%s").lower() \
        and g("git status --porcelain") == "" and "Hello" in (d / "app.py").read_text() \
        and g("git rev-list --count main") == "1"


def s_slug(d):
    w(d, "textlib.py", "def shout(s):\n    return s.upper() + '!'\n")
    w(d, "test_textlib.py", "from textlib import shout\nassert shout('a') == 'A!'\nprint('ok')\n")


def c_slug(d, out):
    code = """
from textlib import slugify, shout
assert slugify('Hello, World!')=='hello-world'
assert slugify('  Multiple   spaces  ')=='multiple-spaces'
assert slugify('Ünïcode café')=='unicode-cafe'
assert slugify('a--b__c')=='a-b-c'
assert slugify('')==''
assert shout('a')=='A!'
print('ok')
"""
    return snippet(d, code).strip() == "ok" and (d / "test_slugify.py").exists() and "slugify" in (d / "test_slugify.py").read_text()


def s_counter(d):
    w(d, "counter.py", "_state = {}\n\ndef incr(name, n=1):\n    _state[name] = _state.get(name, 0) + n\n    return _state[name]\n\n"
                       "def get(name):\n    return _state.get(name, 0)\n\ndef reset():\n    _state.clear()\n")
    w(d, "use.py", "import counter\ncounter.incr('x')\ncounter.incr('x', 4)\nprint(counter.get('x'))\n")


def c_counter(d, out):
    code = """
from counter import Counter
a, b = Counter(), Counter()
a.incr('x'); a.incr('x', 4); b.incr('x', 2)
assert a.get('x')==5 and b.get('x')==2 and a.get('nope')==0
a.reset(); assert a.get('x')==0 and b.get('x')==2
print('ok')
"""
    return snippet(d, code).strip() == "ok" and py(d, "use.py").strip() == "5"


# ---------------------------------------------------------------- hard
def s_todo(d):
    w(d, "SPEC.md", "# todo.py CLI\nPersist to `todos.json` in cwd (list of {id, text, done}).\n"
                    "- `python todo.py add \"text\"` -> prints `added 1` (ids increment from 1, never reused)\n"
                    "- `python todo.py list` -> one line per item: `1 [ ] text` or `1 [x] text`; prints `(empty)` if none\n"
                    "- `python todo.py done ID` -> prints `done ID`; unknown id -> prints `error: no such id` and exit code 1\n"
                    "- `python todo.py rm ID` -> prints `removed ID`; unknown id same error\n")


def c_todo(d, out):
    def run(*a):
        r = subprocess.run([sys.executable, "todo.py", *a], cwd=d, capture_output=True, text=True, timeout=30)
        return r.stdout.strip(), r.returncode
    try:
        f = d / "todos.json"
        f.unlink(missing_ok=True)
        ok = run("list") == ("(empty)", 0)
        ok &= run("add", "buy milk") == ("added 1", 0)
        ok &= run("add", "write code") == ("added 2", 0)
        ok &= run("done", "1") == ("done 1", 0)
        ok &= run("list")[0] == "1 [x] buy milk\n2 [ ] write code"
        ok &= run("rm", "1") == ("removed 1", 0)
        ok &= run("add", "third") == ("added 3", 0)
        o, rc = run("done", "99")
        ok &= (o == "error: no such id" and rc == 1)
        ok &= isinstance(json.loads(f.read_text()), list)
        return bool(ok)
    except Exception:
        return False


def s_recover(d):
    w(d, "data.txt", "10\n20\nabc\n30\n0\n")
    w(d, "proc.py", "vals = [int(l) for l in open('data.txt')]\nprint(sum(100 // v for v in vals))\n")


def c_recover(d, out):
    # model must make it robust: skip non-numeric & zero. expected sum(100//v) for 10,20,30 = 10+5+3 = 18
    return py(d, "proc.py").strip().splitlines()[-1:] == ["18"] and (d / "data.txt").read_text() == "10\n20\nabc\n30\n0\n"


def s_constraint(d):
    w(d, "utils.py", "def is_even(n):\n    return n % 2 == 1\n\ndef double(n):\n    return n * 2\n")
    w(d, "tests/test_utils.py", "from utils import is_even, double\nassert is_even(4) and not is_even(3) and double(2) == 4\nprint('ok')\n")
    d.joinpath(".snap").write_text(digest(d, ["tests/test_utils.py"]))


def c_constraint(d, out):
    files = sorted(p.name for p in d.rglob("*") if p.is_file() and "__pycache__" not in str(p))
    r = subprocess.run([sys.executable, "tests/test_utils.py"], cwd=d, capture_output=True, text=True, timeout=30,
                       env={**__import__("os").environ, "PYTHONPATH": str(d)})
    return r.stdout.strip() == "ok" and digest(d, ["tests/test_utils.py"]) == (d / ".snap").read_text() \
        and files == [".snap", "test_utils.py", "utils.py"] and len((d / "utils.py").read_text().splitlines()) == 5


def s_perf(d):
    w(d, "slow.py", "def count_pairs(xs, target):\n    n = 0\n    for i in range(len(xs)):\n        for j in range(i + 1, len(xs)):\n"
                    "            if xs[i] + xs[j] == target:\n                n += 1\n    return n\n")


def c_perf(d, out):
    code = """
import time, random
from slow import count_pairs
assert count_pairs([1,2,3,4,3],6)==2
assert count_pairs([],1)==0 and count_pairs([3,3,3],6)==3
rnd = random.Random(3); xs = [rnd.randint(0,1000) for _ in range(200000)]
t = time.time(); r = count_pairs(xs, 1000); dt = time.time()-t
from collections import Counter
c = Counter(xs); want = sum(c[a]*c[1000-a] for a in c if a < 1000-a) + (c[500]*(c[500]-1)//2)
assert r == want, (r, want)
print('ok' if dt < 3 else f'slow {dt:.1f}s')
"""
    return snippet(d, code, timeout=120).strip() == "ok"


def c_eval(d, out):
    code = """
from calc import evaluate
cases = {'1+2*3':7,'(1+2)*3':9,'-4+10':6,'2*-3':-6,'10/4':2.5,'2*(3+(4-1))*2':24,'  7 - 2 - 1 ':4,'100/10/5':2,'-(2+3)':-5,'3.5*2':7.0,'2--2':4}
for e,v in cases.items():
    assert abs(evaluate(e)-v)<1e-9,(e,evaluate(e),v)
for bad in ['1+','(1','2**3','abc','']:
    try:
        evaluate(bad); raise SystemExit('no error for '+repr(bad))
    except ValueError: pass
try:
    evaluate('1/0'); raise SystemExit('no zerodiv')
except ZeroDivisionError: pass
print('ok')
"""
    return snippet(d, code).strip() == "ok"


def s_merge(d):
    w(d, "a.json", json.dumps({"name": "svc", "port": 80, "tags": ["a", "b"], "db": {"host": "x", "pool": {"min": 1, "max": 5}}}))
    w(d, "b.json", json.dumps({"port": 8080, "tags": ["b", "c"], "db": {"pool": {"max": 10}, "user": "u"}, "debug": True}))


def c_merge(d, out):
    want = {"name": "svc", "port": 8080, "tags": ["a", "b", "c"], "db": {"host": "x", "pool": {"min": 1, "max": 10}, "user": "u"}, "debug": True}
    try:
        return json.loads((d / "merged.json").read_text()) == want
    except Exception:
        return False


def s_vague(d):
    s_fix(d)


TASKS = [
    # easy
    Task("create_file", "easy", "Create fizzbuzz.py that prints FizzBuzz for 1..15, one item per line.", lambda d: None, c_create),
    Task("fix_bug", "easy", "test_calc.py fails. Fix the bug in calc.py (do not edit the test) and run the test to confirm.", s_fix, c_fix),
    Task("search_answer", "easy", "Which file defines secret_token_generator? Answer with the filename only.", s_search, lambda d, o: "mod7" in o),
    Task("refactor_rename", "easy", "Rename function old_name to new_name everywhere in this directory, including all callers.", s_refactor, c_refactor),
    Task("write_module", "easy", "Create stats.py with median(xs) handling odd and even lengths. Verify it by running a quick check.", lambda d: None, c_stats),
    Task("vague_prompt", "easy", "its broken, fix it", s_vague, c_fix),
    # medium
    Task("multi_bug", "medium", "run_tests.py reports failures. Fix the bugs in the pkg/ modules so it prints ALL PASS. Do not weaken run_tests.py.", s_multibug, c_multibug),
    Task("implement_spec", "medium", "Implement what SPEC.md describes.", s_lru, c_lru),
    Task("big_file_bug", "medium", "python test_big.py reports failing functions. Find and fix the wrong function in big.py (the file is large; don't rewrite it).", s_bigfile, c_bigfile),
    Task("repo_search", "medium", "Somewhere under src/ there is a RETRY_BUDGET setting. What is its value and which file defines it?", s_repo_search, c_repo_search),
    Task("csv_report", "medium", "Write report.py that reads data.csv and writes report.txt with one line per category as 'category,total' (total 2 decimals), sorted by total descending. Run it.", s_csv, c_csv),
    Task("git_workflow", "medium", "Create branch feature/greet, change app.py to print Hello instead of hi, and commit with a message mentioning 'add greeting'. Leave the working tree clean and main untouched.", s_git, c_git),
    Task("add_function_tests", "medium", "Add slugify(s) to textlib.py: lowercase, strip accents to ascii, collapse any run of non-alphanumerics into a single '-', trim '-' at ends. Add tests in test_slugify.py. Keep existing behaviour.", s_slug, c_slug),
    Task("class_refactor", "medium", "Refactor counter.py from module-level state into a Counter class (incr/get/reset per instance) and update use.py so it still prints 5.", s_counter, c_counter),
    # hard
    Task("build_cli", "hard", "Implement what SPEC.md describes.", s_todo, c_todo),
    Task("error_recovery", "hard", "python proc.py crashes. Make it print the sum of 100 // v over the valid positive integers in data.txt, skipping bad lines. Don't modify data.txt.", s_recover, c_recover),
    Task("constraint_follow", "hard", "tests/test_utils.py fails. Fix utils.py by changing exactly one line. Do not modify tests, do not create any new files.", s_constraint, c_constraint),
    Task("perf_optimize", "hard", "count_pairs in slow.py is too slow for 200k elements. Make it fast without changing results (counts index pairs i<j with xs[i]+xs[j]==target).", s_perf, c_perf),
    Task("expr_evaluator", "hard", "Create calc.py with evaluate(expr: str) -> number supporting + - * /, parentheses, unary minus, decimals, whitespace. Invalid syntax raises ValueError, division by zero raises ZeroDivisionError. No eval().", lambda d: None, c_eval),
    Task("json_merge", "hard", "Write merge.py that deep-merges b.json over a.json into merged.json: dicts merge recursively, lists are unioned preserving first-seen order, scalars from b win. Run it.", s_merge, c_merge),
]
