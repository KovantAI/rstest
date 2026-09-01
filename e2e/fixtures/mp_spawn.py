
import multiprocessing as mp

def _sq(x):
    return x * x

def test_spawn_pool():
    ctx = mp.get_context("spawn")
    with ctx.Pool(2) as pool:
        assert pool.map(_sq, [1, 2, 3]) == [1, 4, 9]

def test_spawn_process():
    ctx = mp.get_context("spawn")
    q = ctx.Queue()
    p = ctx.Process(target=q.put, args=(42,))
    p.start()
    assert q.get(timeout=30) == 42
    p.join()
