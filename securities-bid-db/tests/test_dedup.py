"""去重与指纹单元测试。"""
from pipelines.dedup import hamming_distance, is_duplicate, similarity, simhash_text


def test_simhash_identical():
    h1 = simhash_text("服务器采购项目公告")
    h2 = simhash_text("服务器采购项目公告")
    assert similarity(h1, h2) == 1.0


def test_simhash_similar():
    h1 = simhash_text("服务器采购项目公告")
    h2 = simhash_text("服务器采购项目公告（二次）")
    assert similarity(h1, h2) > 0.7


def test_simhash_different():
    h1 = simhash_text("服务器采购项目")
    h2 = simhash_text("网络安全等保测评")
    assert similarity(h1, h2) < 0.9


def test_is_duplicate():
    h1 = simhash_text("服务器采购项目")
    h2 = simhash_text("服务器采购项目（二次公告）")
    assert is_duplicate(h1, [h2], threshold=0.5) is True


def test_hamming_distance():
    assert hamming_distance(0, 0) == 0
    assert hamming_distance(0b1010, 0b1000) == 1
