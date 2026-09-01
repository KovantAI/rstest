
def test_ident_present(request):
    assert request.config._follower_ident.startswith("follower_gw")


def test_workerinput_kept(request):
    assert request.config.workerinput["follower_ident"] == request.config._follower_ident
