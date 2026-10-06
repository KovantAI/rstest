def test_passes():
    assert 1 + 1 == 2

def test_fails():
    assert 1 + 1 == 3, "math broke"

def test_error():
    raise RuntimeError("boom")

def helper():  # not a test
    raise AssertionError("should never run")
