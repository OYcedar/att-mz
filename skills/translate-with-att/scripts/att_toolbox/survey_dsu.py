"""调查关系图使用的并查集。"""


class DisjointSet:
    def __init__(self, size: int) -> None:
        self._parent = list(range(size))
        self._rank = [0] * size

    def find(self, value: int) -> int:
        root = value
        while self._parent[root] != root:
            root = self._parent[root]
        while self._parent[value] != value:
            parent = self._parent[value]
            self._parent[value] = root
            value = parent
        return root

    def union(self, left: int, right: int) -> None:
        first = self.find(left)
        second = self.find(right)
        if first == second:
            return
        if self._rank[first] < self._rank[second]:
            first, second = second, first
        self._parent[second] = first
        if self._rank[first] == self._rank[second]:
            self._rank[first] += 1
