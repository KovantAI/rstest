import pytest

@pytest.fixture
def bad_td():
    yield
    raise RuntimeError("teardown boom")

@pytest.fixture
def bad_setup():
    raise ValueError("setup boom")

def test_pass_td_err(bad_td):
    pass

def test_fail_td_err(bad_td):
    assert 1 == 2

def test_setup_err(bad_setup):
    pass

def test_ok():
    pass

@pytest.mark.skip(reason="nope")
def test_skip():
    pass

@pytest.mark.xfail(reason="known")
def test_xf():
    assert 0

@pytest.mark.xfail(reason="lucky")
def test_xp():
    pass

# Wide chars: pytest trims the message by terminal columns (wcswidth).
def test_wide():
    assert False, "日本語のメッセージ" * 20
