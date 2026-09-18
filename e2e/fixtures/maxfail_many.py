
# Six instant-failing tests: at -n 1 --maxfail=K the pool must report EXACTLY
# K failures then stop (no overshoot with one worker), leaving the rest unrun.

def test_f1(): assert False
def test_f2(): assert False
def test_f3(): assert False
def test_f4(): assert False
def test_f5(): assert False
def test_f6(): assert False
