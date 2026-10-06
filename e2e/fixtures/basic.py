
def test_passes():
    assert 1 + 1 == 2

def test_fails():
    assert 1 + 1 == 3, "math broke"

def test_error():
    raise RuntimeError("boom")

def test_also_passes():
    assert "abc".upper() == "ABC"
