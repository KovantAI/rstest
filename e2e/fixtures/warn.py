
import warnings

def test_warns():
    warnings.warn("noisy thing", UserWarning)

def test_clean():
    assert True
