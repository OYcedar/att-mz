"""调查关系图使用的并查集。"""


class DisjointSet:
    def __init__(self, size: int) -> None:
        self._parent = list(range(size))

    def find(self, value: int) -> int:
        parent = self._parent[value]
        if parent != value:
            parent = self.find(parent)
            self._parent[value] = parent
        return parent

    def union(self, left: int, right: int) -> None:
        first = self.find(left)
        second = self.find(right)
        if first != second:
            self._parent[second] = first
